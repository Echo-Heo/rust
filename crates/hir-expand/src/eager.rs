//! Eager expansion related utils
//!
//! Here is a dump of a discussion from Vadim Petrochenkov about Eager Expansion and
//! Its name resolution :
//!
//! > Eagerly expanded macros (and also macros eagerly expanded by eagerly expanded macros,
//! > which actually happens in practice too!) are resolved at the location of the "root" macro
//! > that performs the eager expansion on its arguments.
//! > If some name cannot be resolved at the eager expansion time it's considered unresolved,
//! > even if becomes available later (e.g. from a glob import or other macro).
//!
//! > Eagerly expanded macros don't add anything to the module structure of the crate and
//! > don't build any speculative module structures, i.e. they are expanded in a "flat"
//! > way even if tokens in them look like modules.
//!
//! > In other words, it kinda works for simple cases for which it was originally intended,
//! > and we need to live with it because it's available on stable and widely relied upon.
//!
//!
//! See the full discussion : <https://rust-lang.zulipchat.com/#narrow/stream/131828-t-compiler/topic/Eager.20expansion.20of.20built-in.20macros>
use base_db::{span::SyntaxContextId, CrateId, FileId};
use rustc_hash::FxHashMap;
use syntax::{ted, Parse, SyntaxNode, TextRange, TextSize, WalkEvent};
use triomphe::Arc;

use crate::{
    ast::{self, AstNode},
    db::ExpandDatabase,
    mod_path::ModPath,
    span::{RealSpanMap, SpanMapRef},
    EagerCallInfo, ExpandError, ExpandResult, ExpandTo, ExpansionSpanMap, InFile, MacroCallId,
    MacroCallKind, MacroCallLoc, MacroDefId, MacroDefKind,
};

pub fn expand_eager_macro_input(
    db: &dyn ExpandDatabase,
    krate: CrateId,
    macro_call: InFile<ast::MacroCall>,
    def: MacroDefId,
    call_site: SyntaxContextId,
    resolver: &dyn Fn(ModPath) -> Option<MacroDefId>,
) -> ExpandResult<Option<MacroCallId>> {
    let ast_map = db.ast_id_map(macro_call.file_id);
    // the expansion which the ast id map is built upon has no whitespace, so the offsets are wrong as macro_call is from the token tree that has whitespace!
    let call_id = InFile::new(macro_call.file_id, ast_map.ast_id(&macro_call.value));
    let expand_to = ExpandTo::from_call_site(&macro_call.value);

    // Note:
    // When `lazy_expand` is called, its *parent* file must already exist.
    // Here we store an eager macro id for the argument expanded subtree
    // for that purpose.
    let arg_id = db.intern_macro_call(MacroCallLoc {
        def,
        krate,
        eager: None,
        kind: MacroCallKind::FnLike { ast_id: call_id, expand_to: ExpandTo::Expr },
        call_site,
    });
    let ExpandResult { value: (arg_exp, arg_exp_map), err: parse_err } =
        db.parse_macro_expansion(arg_id.as_macro_file());

    let ExpandResult { value: expanded_eager_input, err } = {
        eager_macro_recur(
            db,
            &arg_exp_map,
            InFile::new(arg_id.as_file(), arg_exp.syntax_node()),
            krate,
            call_site,
            resolver,
        )
    };
    let err = parse_err.or(err);

    let Some((expanded_eager_input, _mapping)) = expanded_eager_input else {
        return ExpandResult { value: None, err };
    };

    // FIXME: Spans!
    let mut subtree =
        mbe::syntax_node_to_token_tree(&expanded_eager_input, RealSpanMap::absolute(FileId::BOGUS));

    subtree.delimiter = crate::tt::Delimiter::DUMMY_INVISIBLE;

    let loc = MacroCallLoc {
        def,
        krate,
        eager: Some(Box::new(EagerCallInfo { arg: Arc::new(subtree), arg_id, error: err.clone() })),
        kind: MacroCallKind::FnLike { ast_id: call_id, expand_to },
        call_site,
    };

    ExpandResult { value: Some(db.intern_macro_call(loc)), err }
}

fn lazy_expand(
    db: &dyn ExpandDatabase,
    def: &MacroDefId,
    macro_call: InFile<ast::MacroCall>,
    krate: CrateId,
    call_site: SyntaxContextId,
) -> ExpandResult<(InFile<Parse<SyntaxNode>>, Arc<ExpansionSpanMap>)> {
    let ast_id = db.ast_id_map(macro_call.file_id).ast_id(&macro_call.value);

    let expand_to = ExpandTo::from_call_site(&macro_call.value);
    let ast_id = macro_call.with_value(ast_id);
    let id = def.as_lazy_macro(
        db,
        krate,
        MacroCallKind::FnLike { ast_id, expand_to },
        // FIXME: This is wrong
        call_site,
    );
    let macro_file = id.as_macro_file();

    db.parse_macro_expansion(macro_file)
        .map(|parse| (InFile::new(macro_file.into(), parse.0), parse.1))
}

fn eager_macro_recur(
    db: &dyn ExpandDatabase,
    span_map: &ExpansionSpanMap,
    curr: InFile<SyntaxNode>,
    krate: CrateId,
    call_site: SyntaxContextId,
    macro_resolver: &dyn Fn(ModPath) -> Option<MacroDefId>,
) -> ExpandResult<Option<(SyntaxNode, FxHashMap<TextRange, TextRange>)>> {
    let original = curr.value.clone_for_update();
    let mut mapping = FxHashMap::default();

    let mut replacements = Vec::new();

    // FIXME: We only report a single error inside of eager expansions
    let mut error = None;
    let mut offset = 0i32;
    let apply_offset = |it: TextSize, offset: i32| {
        TextSize::from(u32::try_from(offset + u32::from(it) as i32).unwrap_or_default())
    };
    let mut children = original.preorder_with_tokens();

    // Collect replacement
    while let Some(child) = children.next() {
        let WalkEvent::Enter(child) = child else { continue };
        let call = match child {
            syntax::NodeOrToken::Node(node) => match ast::MacroCall::cast(node) {
                Some(it) => {
                    children.skip_subtree();
                    it
                }
                None => continue,
            },
            syntax::NodeOrToken::Token(t) => {
                mapping.insert(
                    TextRange::new(
                        apply_offset(t.text_range().start(), offset),
                        apply_offset(t.text_range().end(), offset),
                    ),
                    t.text_range(),
                );
                continue;
            }
        };
        let def = match call
            .path()
            .and_then(|path| ModPath::from_src(db, path, SpanMapRef::ExpansionSpanMap(span_map)))
        {
            Some(path) => match macro_resolver(path.clone()) {
                Some(def) => def,
                None => {
                    error =
                        Some(ExpandError::other(format!("unresolved macro {}", path.display(db))));
                    continue;
                }
            },
            None => {
                error = Some(ExpandError::other("malformed macro invocation"));
                continue;
            }
        };
        let ExpandResult { value, err } = match def.kind {
            MacroDefKind::BuiltInEager(..) => {
                let ExpandResult { value, err } = expand_eager_macro_input(
                    db,
                    krate,
                    curr.with_value(call.clone()),
                    def,
                    // FIXME: This call site is not quite right I think? We probably need to mark it?
                    call_site,
                    macro_resolver,
                );
                match value {
                    Some(call_id) => {
                        let ExpandResult { value, err: err2 } =
                            db.parse_macro_expansion(call_id.as_macro_file());

                        // if let Some(tt) = call.token_tree() {
                        // let call_tt_start = tt.syntax().text_range().start();
                        // let call_start =
                        //     apply_offset(call.syntax().text_range().start(), offset);
                        // if let Some((_, arg_map, _)) = db.macro_arg(call_id).value.as_deref() {
                        //     mapping.extend(arg_map.entries().filter_map(|(tid, range)| {
                        //         value
                        //             .1
                        //             .first_range_by_token(tid, syntax::SyntaxKind::TOMBSTONE)
                        //             .map(|r| (r + call_start, range + call_tt_start))
                        //     }));
                        // }
                        // }

                        ExpandResult {
                            value: Some(value.0.syntax_node().clone_for_update()),
                            err: err.or(err2),
                        }
                    }
                    None => ExpandResult { value: None, err },
                }
            }
            MacroDefKind::Declarative(_)
            | MacroDefKind::BuiltIn(..)
            | MacroDefKind::BuiltInAttr(..)
            | MacroDefKind::BuiltInDerive(..)
            | MacroDefKind::ProcMacro(..) => {
                let ExpandResult { value: (parse, tm), err } =
                    lazy_expand(db, &def, curr.with_value(call.clone()), krate, call_site);

                // replace macro inside
                let ExpandResult { value, err: error } = eager_macro_recur(
                    db,
                    &tm,
                    // FIXME: We discard parse errors here
                    parse.as_ref().map(|it| it.syntax_node()),
                    krate,
                    call_site,
                    macro_resolver,
                );
                let err = err.or(error);

                // if let Some(tt) = call.token_tree() {
                // let decl_mac = if let MacroDefKind::Declarative(ast_id) = def.kind {
                //     Some(db.decl_macro_expander(def.krate, ast_id))
                // } else {
                //     None
                // };
                // let call_tt_start = tt.syntax().text_range().start();
                // let call_start = apply_offset(call.syntax().text_range().start(), offset);
                // if let Some((_tt, arg_map, _)) = parse
                //     .file_id
                //     .macro_file()
                //     .and_then(|id| db.macro_arg(id.macro_call_id).value)
                //     .as_deref()
                // {
                //     mapping.extend(arg_map.entries().filter_map(|(tid, range)| {
                //         tm.first_range_by_token(
                //             decl_mac.as_ref().map(|it| it.map_id_down(tid)).unwrap_or(tid),
                //             syntax::SyntaxKind::TOMBSTONE,
                //         )
                //         .map(|r| (r + call_start, range + call_tt_start))
                //     }));
                // }
                // }
                // FIXME: Do we need to re-use _m here?
                ExpandResult { value: value.map(|(n, _m)| n), err }
            }
        };
        if err.is_some() {
            error = err;
        }
        // check if the whole original syntax is replaced
        if call.syntax() == &original {
            return ExpandResult { value: value.zip(Some(mapping)), err: error };
        }

        if let Some(insert) = value {
            offset += u32::from(insert.text_range().len()) as i32
                - u32::from(call.syntax().text_range().len()) as i32;
            replacements.push((call, insert));
        }
    }

    replacements.into_iter().rev().for_each(|(old, new)| ted::replace(old.syntax(), new));
    ExpandResult { value: Some((original, mapping)), err: error }
}
