use std::{
    sync::Arc,
    ops::Index,
};

use test_utils::tested_by;
use ra_arena::{Arena, impl_arena_id, RawId, map::ArenaMap};
use ra_syntax::{
    AstNode, SourceFile, AstPtr, TreeArc,
    ast::{self, NameOwner, AttrsOwner},
};

use crate::{
    DefDatabase, Name, AsName, Path, HirFileId, ModuleSource,
    SourceFileItemId, SourceFileItems, FileAstId,
};

/// `RawItems` is a set of top-level items in a file (except for impls).
///
/// It is the input to name resolution algorithm. `RawItems` are not invalidated
/// on most edits.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RawItems {
    modules: Arena<Module, ModuleData>,
    imports: Arena<ImportId, ImportData>,
    defs: Arena<Def, DefData>,
    macros: Arena<Macro, MacroData>,
    /// items for top-level module
    items: Vec<RawItem>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSourceMap {
    map: ArenaMap<ImportId, AstPtr<ast::PathSegment>>,
}

impl ImportSourceMap {
    fn insert(&mut self, import: ImportId, segment: &ast::PathSegment) {
        self.map.insert(import, AstPtr::new(segment))
    }

    pub(crate) fn get(&self, source: &ModuleSource, import: ImportId) -> TreeArc<ast::PathSegment> {
        let file = match source {
            ModuleSource::SourceFile(file) => &*file,
            ModuleSource::Module(m) => m.syntax().ancestors().find_map(SourceFile::cast).unwrap(),
        };

        self.map[import].to_node(file).to_owned()
    }
}

impl RawItems {
    pub(crate) fn raw_items_query(db: &impl DefDatabase, file_id: HirFileId) -> Arc<RawItems> {
        db.raw_items_with_source_map(file_id).0
    }

    pub(crate) fn raw_items_with_source_map_query(
        db: &impl DefDatabase,
        file_id: HirFileId,
    ) -> (Arc<RawItems>, Arc<ImportSourceMap>) {
        let mut collector = RawItemsCollector {
            raw_items: RawItems::default(),
            source_file_items: db.file_items(file_id.into()),
            source_map: ImportSourceMap::default(),
        };
        let source_file = db.hir_parse(file_id);
        collector.process_module(None, &*source_file);
        (Arc::new(collector.raw_items), Arc::new(collector.source_map))
    }

    pub(super) fn items(&self) -> &[RawItem] {
        &self.items
    }
}

impl Index<Module> for RawItems {
    type Output = ModuleData;
    fn index(&self, idx: Module) -> &ModuleData {
        &self.modules[idx]
    }
}

impl Index<ImportId> for RawItems {
    type Output = ImportData;
    fn index(&self, idx: ImportId) -> &ImportData {
        &self.imports[idx]
    }
}

impl Index<Def> for RawItems {
    type Output = DefData;
    fn index(&self, idx: Def) -> &DefData {
        &self.defs[idx]
    }
}

impl Index<Macro> for RawItems {
    type Output = MacroData;
    fn index(&self, idx: Macro) -> &MacroData {
        &self.macros[idx]
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum RawItem {
    Module(Module),
    Import(ImportId),
    Def(Def),
    Macro(Macro),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Module(RawId);
impl_arena_id!(Module);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ModuleData {
    Declaration { name: Name, ast_id: FileAstId<ast::Module> },
    Definition { name: Name, ast_id: FileAstId<ast::Module>, items: Vec<RawItem> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImportId(RawId);
impl_arena_id!(ImportId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportData {
    pub(super) path: Path,
    pub(super) alias: Option<Name>,
    pub(super) is_glob: bool,
    pub(super) is_prelude: bool,
    pub(super) is_extern_crate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Def(RawId);
impl_arena_id!(Def);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DefData {
    pub(super) source_item_id: SourceFileItemId,
    pub(super) name: Name,
    pub(super) kind: DefKind,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum DefKind {
    Function,
    Struct,
    Enum,
    Const,
    Static,
    Trait,
    TypeAlias,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Macro(RawId);
impl_arena_id!(Macro);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct MacroData {
    pub(super) ast_id: FileAstId<ast::MacroCall>,
    pub(super) path: Path,
    pub(super) name: Option<Name>,
    pub(super) export: bool,
}

struct RawItemsCollector {
    raw_items: RawItems,
    source_file_items: Arc<SourceFileItems>,
    source_map: ImportSourceMap,
}

impl RawItemsCollector {
    fn process_module(&mut self, current_module: Option<Module>, body: &impl ast::ModuleItemOwner) {
        for item_or_macro in body.items_with_macros() {
            match item_or_macro {
                ast::ItemOrMacro::Macro(m) => self.add_macro(current_module, m),
                ast::ItemOrMacro::Item(item) => self.add_item(current_module, item),
            }
        }
    }

    fn add_item(&mut self, current_module: Option<Module>, item: &ast::ModuleItem) {
        let (kind, name) = match item.kind() {
            ast::ModuleItemKind::Module(module) => {
                self.add_module(current_module, module);
                return;
            }
            ast::ModuleItemKind::UseItem(use_item) => {
                self.add_use_item(current_module, use_item);
                return;
            }
            ast::ModuleItemKind::ExternCrateItem(extern_crate) => {
                self.add_extern_crate_item(current_module, extern_crate);
                return;
            }
            ast::ModuleItemKind::ImplBlock(_) => {
                // impls don't participate in name resolution
                return;
            }
            ast::ModuleItemKind::StructDef(it) => (DefKind::Struct, it.name()),
            ast::ModuleItemKind::EnumDef(it) => (DefKind::Enum, it.name()),
            ast::ModuleItemKind::FnDef(it) => (DefKind::Function, it.name()),
            ast::ModuleItemKind::TraitDef(it) => (DefKind::Trait, it.name()),
            ast::ModuleItemKind::TypeAliasDef(it) => (DefKind::TypeAlias, it.name()),
            ast::ModuleItemKind::ConstDef(it) => (DefKind::Const, it.name()),
            ast::ModuleItemKind::StaticDef(it) => (DefKind::Static, it.name()),
        };
        if let Some(name) = name {
            let name = name.as_name();
            let source_item_id = self.source_file_items.id_of_unchecked(item.syntax());
            let def = self.raw_items.defs.alloc(DefData { name, kind, source_item_id });
            self.push_item(current_module, RawItem::Def(def))
        }
    }

    fn add_module(&mut self, current_module: Option<Module>, module: &ast::Module) {
        let name = match module.name() {
            Some(it) => it.as_name(),
            None => return,
        };
        let ast_id = self.source_file_items.ast_id(module);
        if module.has_semi() {
            let item = self.raw_items.modules.alloc(ModuleData::Declaration { name, ast_id });
            self.push_item(current_module, RawItem::Module(item));
            return;
        }

        if let Some(item_list) = module.item_list() {
            let item = self.raw_items.modules.alloc(ModuleData::Definition {
                name,
                ast_id,
                items: Vec::new(),
            });
            self.process_module(Some(item), item_list);
            self.push_item(current_module, RawItem::Module(item));
            return;
        }
        tested_by!(name_res_works_for_broken_modules);
    }

    fn add_use_item(&mut self, current_module: Option<Module>, use_item: &ast::UseItem) {
        let is_prelude = use_item.has_atom_attr("prelude_import");

        Path::expand_use_item(use_item, |path, segment, alias| {
            let import = self.raw_items.imports.alloc(ImportData {
                path,
                alias,
                is_glob: segment.is_none(),
                is_prelude,
                is_extern_crate: false,
            });
            if let Some(segment) = segment {
                self.source_map.insert(import, segment)
            }
            self.push_item(current_module, RawItem::Import(import))
        })
    }

    fn add_extern_crate_item(
        &mut self,
        current_module: Option<Module>,
        extern_crate: &ast::ExternCrateItem,
    ) {
        if let Some(name_ref) = extern_crate.name_ref() {
            let path = Path::from_name_ref(name_ref);
            let alias = extern_crate.alias().and_then(|a| a.name()).map(AsName::as_name);
            let import = self.raw_items.imports.alloc(ImportData {
                path,
                alias,
                is_glob: false,
                is_prelude: false,
                is_extern_crate: true,
            });
            self.push_item(current_module, RawItem::Import(import))
        }
    }

    fn add_macro(&mut self, current_module: Option<Module>, m: &ast::MacroCall) {
        let path = match m.path().and_then(Path::from_ast) {
            Some(it) => it,
            _ => return,
        };

        let name = m.name().map(|it| it.as_name());
        let ast_id = self.source_file_items.ast_id(m);
        let export = m.has_atom_attr("macro_export");
        let m = self.raw_items.macros.alloc(MacroData { ast_id, path, name, export });
        self.push_item(current_module, RawItem::Macro(m));
    }

    fn push_item(&mut self, current_module: Option<Module>, item: RawItem) {
        match current_module {
            Some(module) => match &mut self.raw_items.modules[module] {
                ModuleData::Definition { items, .. } => items,
                ModuleData::Declaration { .. } => unreachable!(),
            },
            None => &mut self.raw_items.items,
        }
        .push(item)
    }
}
