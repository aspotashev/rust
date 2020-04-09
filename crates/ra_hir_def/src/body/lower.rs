//! Transforms `ast::Expr` into an equivalent `hir_def::expr::Expr`
//! representation.

use either::Either;

use hir_expand::{
    hygiene::Hygiene,
    name::{name, AsName, Name},
    MacroDefId, MacroDefKind,
};
use ra_arena::Arena;
use ra_syntax::{
    ast::{
        self, ArgListOwner, ArrayExprKind, LiteralKind, LoopBodyOwner, ModuleItemOwner, NameOwner,
        SlicePatComponents, TypeAscriptionOwner,
    },
    AstNode, AstPtr,
};
use test_utils::tested_by;

use super::{ExprSource, PatSource};
use crate::{
    adt::StructKind,
    attr::Attrs,
    body::{Body, BodySourceMap, Expander, PatPtr, SyntheticSyntax},
    builtin_type::{BuiltinFloat, BuiltinInt},
    db::DefDatabase,
    expr::{
        dummy_expr_id, ArithOp, Array, BinaryOp, BindingAnnotation, CmpOp, Expr, ExprId, Literal,
        LogicOp, MatchArm, Ordering, Pat, PatId, RecordFieldPat, RecordLitField, Statement,
    },
    item_scope::BuiltinShadowMode,
    path::GenericArgs,
    path::Path,
    type_ref::{Mutability, TypeRef},
    AdtId, ConstLoc, ContainerId, DefWithBodyId, EnumLoc, FunctionLoc, HasModule, Intern,
    ModuleDefId, StaticLoc, StructLoc, TraitLoc, TypeAliasLoc, UnionLoc,
};

pub(super) fn lower(
    db: &dyn DefDatabase,
    def: DefWithBodyId,
    expander: Expander,
    params: Option<ast::ParamList>,
    body: Option<ast::Expr>,
) -> (Body, BodySourceMap) {
    ExprCollector {
        db,
        def,
        expander,
        source_map: BodySourceMap::default(),
        body: Body {
            exprs: Arena::default(),
            pats: Arena::default(),
            params: Vec::new(),
            body_expr: dummy_expr_id(),
            item_scope: Default::default(),
        },
    }
    .collect(params, body)
}

struct ExprCollector<'a> {
    db: &'a dyn DefDatabase,
    def: DefWithBodyId,
    expander: Expander,

    body: Body,
    source_map: BodySourceMap,
}

impl ExprCollector<'_> {
    fn collect(
        mut self,
        param_list: Option<ast::ParamList>,
        body: Option<ast::Expr>,
    ) -> (Body, BodySourceMap) {
        if let Some(param_list) = param_list {
            if let Some(self_param) = param_list.self_param() {
                let ptr = AstPtr::new(&self_param);
                let param_pat = self.alloc_pat(
                    Pat::Bind {
                        name: name![self],
                        mode: BindingAnnotation::Unannotated,
                        subpat: None,
                    },
                    Either::Right(ptr),
                );
                self.body.params.push(param_pat);
            }

            for param in param_list.params() {
                let pat = match param.pat() {
                    None => continue,
                    Some(pat) => pat,
                };
                let param_pat = self.collect_pat(pat);
                self.body.params.push(param_pat);
            }
        };

        self.body.body_expr = self.collect_expr_opt(body);
        (self.body, self.source_map)
    }

    fn alloc_expr(&mut self, expr: Expr, ptr: AstPtr<ast::Expr>) -> ExprId {
        let ptr = Either::Left(ptr);
        let src = self.expander.to_source(ptr);
        let id = self.make_expr(expr, Ok(src));
        self.source_map.expr_map.insert(src, id);
        id
    }
    // desugared exprs don't have ptr, that's wrong and should be fixed
    // somehow.
    fn alloc_expr_desugared(&mut self, expr: Expr) -> ExprId {
        self.make_expr(expr, Err(SyntheticSyntax))
    }
    fn alloc_expr_field_shorthand(&mut self, expr: Expr, ptr: AstPtr<ast::RecordField>) -> ExprId {
        let ptr = Either::Right(ptr);
        let src = self.expander.to_source(ptr);
        let id = self.make_expr(expr, Ok(src));
        self.source_map.expr_map.insert(src, id);
        id
    }
    fn empty_block(&mut self) -> ExprId {
        self.alloc_expr_desugared(Expr::Block { statements: Vec::new(), tail: None })
    }
    fn missing_expr(&mut self) -> ExprId {
        self.alloc_expr_desugared(Expr::Missing)
    }
    fn make_expr(&mut self, expr: Expr, src: Result<ExprSource, SyntheticSyntax>) -> ExprId {
        let id = self.body.exprs.alloc(expr);
        self.source_map.expr_map_back.insert(id, src);
        id
    }

    fn alloc_pat(&mut self, pat: Pat, ptr: PatPtr) -> PatId {
        let src = self.expander.to_source(ptr);
        let id = self.make_pat(pat, Ok(src));
        self.source_map.pat_map.insert(src, id);
        id
    }
    fn missing_pat(&mut self) -> PatId {
        self.make_pat(Pat::Missing, Err(SyntheticSyntax))
    }
    fn make_pat(&mut self, pat: Pat, src: Result<PatSource, SyntheticSyntax>) -> PatId {
        let id = self.body.pats.alloc(pat);
        self.source_map.pat_map_back.insert(id, src);
        id
    }

    fn collect_expr(&mut self, expr: ast::Expr) -> ExprId {
        let syntax_ptr = AstPtr::new(&expr);
        match expr {
            ast::Expr::IfExpr(e) => {
                let then_branch = self.collect_block_opt(e.then_branch());

                let else_branch = e.else_branch().map(|b| match b {
                    ast::ElseBranch::Block(it) => self.collect_block(it),
                    ast::ElseBranch::IfExpr(elif) => {
                        let expr: ast::Expr = ast::Expr::cast(elif.syntax().clone()).unwrap();
                        self.collect_expr(expr)
                    }
                });

                let condition = match e.condition() {
                    None => self.missing_expr(),
                    Some(condition) => match condition.pat() {
                        None => self.collect_expr_opt(condition.expr()),
                        // if let -- desugar to match
                        Some(pat) => {
                            let pat = self.collect_pat(pat);
                            let match_expr = self.collect_expr_opt(condition.expr());
                            let placeholder_pat = self.missing_pat();
                            let arms = vec![
                                MatchArm { pat, expr: then_branch, guard: None },
                                MatchArm {
                                    pat: placeholder_pat,
                                    expr: else_branch.unwrap_or_else(|| self.empty_block()),
                                    guard: None,
                                },
                            ];
                            return self
                                .alloc_expr(Expr::Match { expr: match_expr, arms }, syntax_ptr);
                        }
                    },
                };

                self.alloc_expr(Expr::If { condition, then_branch, else_branch }, syntax_ptr)
            }
            ast::Expr::TryBlockExpr(e) => {
                let body = self.collect_block_opt(e.body());
                self.alloc_expr(Expr::TryBlock { body }, syntax_ptr)
            }
            ast::Expr::BlockExpr(e) => self.collect_block(e),
            ast::Expr::LoopExpr(e) => {
                let body = self.collect_block_opt(e.loop_body());
                self.alloc_expr(Expr::Loop { body }, syntax_ptr)
            }
            ast::Expr::WhileExpr(e) => {
                let body = self.collect_block_opt(e.loop_body());

                let condition = match e.condition() {
                    None => self.missing_expr(),
                    Some(condition) => match condition.pat() {
                        None => self.collect_expr_opt(condition.expr()),
                        // if let -- desugar to match
                        Some(pat) => {
                            tested_by!(infer_resolve_while_let);
                            let pat = self.collect_pat(pat);
                            let match_expr = self.collect_expr_opt(condition.expr());
                            let placeholder_pat = self.missing_pat();
                            let break_ = self.alloc_expr_desugared(Expr::Break { expr: None });
                            let arms = vec![
                                MatchArm { pat, expr: body, guard: None },
                                MatchArm { pat: placeholder_pat, expr: break_, guard: None },
                            ];
                            let match_expr =
                                self.alloc_expr_desugared(Expr::Match { expr: match_expr, arms });
                            return self.alloc_expr(Expr::Loop { body: match_expr }, syntax_ptr);
                        }
                    },
                };

                self.alloc_expr(Expr::While { condition, body }, syntax_ptr)
            }
            ast::Expr::ForExpr(e) => {
                let iterable = self.collect_expr_opt(e.iterable());
                let pat = self.collect_pat_opt(e.pat());
                let body = self.collect_block_opt(e.loop_body());
                self.alloc_expr(Expr::For { iterable, pat, body }, syntax_ptr)
            }
            ast::Expr::CallExpr(e) => {
                let callee = self.collect_expr_opt(e.expr());
                let args = if let Some(arg_list) = e.arg_list() {
                    arg_list.args().map(|e| self.collect_expr(e)).collect()
                } else {
                    Vec::new()
                };
                self.alloc_expr(Expr::Call { callee, args }, syntax_ptr)
            }
            ast::Expr::MethodCallExpr(e) => {
                let receiver = self.collect_expr_opt(e.expr());
                let args = if let Some(arg_list) = e.arg_list() {
                    arg_list.args().map(|e| self.collect_expr(e)).collect()
                } else {
                    Vec::new()
                };
                let method_name = e.name_ref().map(|nr| nr.as_name()).unwrap_or_else(Name::missing);
                let generic_args = e.type_arg_list().and_then(GenericArgs::from_ast);
                self.alloc_expr(
                    Expr::MethodCall { receiver, method_name, args, generic_args },
                    syntax_ptr,
                )
            }
            ast::Expr::MatchExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let arms = if let Some(match_arm_list) = e.match_arm_list() {
                    match_arm_list
                        .arms()
                        .map(|arm| MatchArm {
                            pat: self.collect_pat_opt(arm.pat()),
                            expr: self.collect_expr_opt(arm.expr()),
                            guard: arm
                                .guard()
                                .and_then(|guard| guard.expr())
                                .map(|e| self.collect_expr(e)),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                self.alloc_expr(Expr::Match { expr, arms }, syntax_ptr)
            }
            ast::Expr::PathExpr(e) => {
                let path = e
                    .path()
                    .and_then(|path| self.expander.parse_path(path))
                    .map(Expr::Path)
                    .unwrap_or(Expr::Missing);
                self.alloc_expr(path, syntax_ptr)
            }
            ast::Expr::ContinueExpr(_e) => {
                // FIXME: labels
                self.alloc_expr(Expr::Continue, syntax_ptr)
            }
            ast::Expr::BreakExpr(e) => {
                let expr = e.expr().map(|e| self.collect_expr(e));
                self.alloc_expr(Expr::Break { expr }, syntax_ptr)
            }
            ast::Expr::ParenExpr(e) => {
                let inner = self.collect_expr_opt(e.expr());
                // make the paren expr point to the inner expression as well
                let src = self.expander.to_source(Either::Left(syntax_ptr));
                self.source_map.expr_map.insert(src, inner);
                inner
            }
            ast::Expr::ReturnExpr(e) => {
                let expr = e.expr().map(|e| self.collect_expr(e));
                self.alloc_expr(Expr::Return { expr }, syntax_ptr)
            }
            ast::Expr::RecordLit(e) => {
                let crate_graph = self.db.crate_graph();
                let path = e.path().and_then(|path| self.expander.parse_path(path));
                let mut field_ptrs = Vec::new();
                let record_lit = if let Some(nfl) = e.record_field_list() {
                    let fields = nfl
                        .fields()
                        .inspect(|field| field_ptrs.push(AstPtr::new(field)))
                        .filter_map(|field| {
                            let module_id = ContainerId::DefWithBodyId(self.def).module(self.db);
                            let attrs = Attrs::new(
                                &field,
                                &Hygiene::new(self.db.upcast(), self.expander.current_file_id),
                            );

                            if !attrs.is_cfg_enabled(&crate_graph[module_id.krate].cfg_options) {
                                return None;
                            }

                            Some(RecordLitField {
                                name: field
                                    .name_ref()
                                    .map(|nr| nr.as_name())
                                    .unwrap_or_else(Name::missing),
                                expr: if let Some(e) = field.expr() {
                                    self.collect_expr(e)
                                } else if let Some(nr) = field.name_ref() {
                                    // field shorthand
                                    self.alloc_expr_field_shorthand(
                                        Expr::Path(Path::from_name_ref(&nr)),
                                        AstPtr::new(&field),
                                    )
                                } else {
                                    self.missing_expr()
                                },
                            })
                        })
                        .collect();
                    let spread = nfl.spread().map(|s| self.collect_expr(s));
                    Expr::RecordLit { path, fields, spread }
                } else {
                    Expr::RecordLit { path, fields: Vec::new(), spread: None }
                };

                let res = self.alloc_expr(record_lit, syntax_ptr);
                for (i, ptr) in field_ptrs.into_iter().enumerate() {
                    self.source_map.field_map.insert((res, i), ptr);
                }
                res
            }
            ast::Expr::FieldExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let name = match e.field_access() {
                    Some(kind) => kind.as_name(),
                    _ => Name::missing(),
                };
                self.alloc_expr(Expr::Field { expr, name }, syntax_ptr)
            }
            ast::Expr::AwaitExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                self.alloc_expr(Expr::Await { expr }, syntax_ptr)
            }
            ast::Expr::TryExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                self.alloc_expr(Expr::Try { expr }, syntax_ptr)
            }
            ast::Expr::CastExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let type_ref = TypeRef::from_ast_opt(e.type_ref());
                self.alloc_expr(Expr::Cast { expr, type_ref }, syntax_ptr)
            }
            ast::Expr::RefExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let mutability = Mutability::from_mutable(e.mut_token().is_some());
                self.alloc_expr(Expr::Ref { expr, mutability }, syntax_ptr)
            }
            ast::Expr::PrefixExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                if let Some(op) = e.op_kind() {
                    self.alloc_expr(Expr::UnaryOp { expr, op }, syntax_ptr)
                } else {
                    self.alloc_expr(Expr::Missing, syntax_ptr)
                }
            }
            ast::Expr::LambdaExpr(e) => {
                let mut args = Vec::new();
                let mut arg_types = Vec::new();
                if let Some(pl) = e.param_list() {
                    for param in pl.params() {
                        let pat = self.collect_pat_opt(param.pat());
                        let type_ref = param.ascribed_type().map(TypeRef::from_ast);
                        args.push(pat);
                        arg_types.push(type_ref);
                    }
                }
                let ret_type = e.ret_type().and_then(|r| r.type_ref()).map(TypeRef::from_ast);
                let body = self.collect_expr_opt(e.body());
                self.alloc_expr(Expr::Lambda { args, arg_types, ret_type, body }, syntax_ptr)
            }
            ast::Expr::BinExpr(e) => {
                let lhs = self.collect_expr_opt(e.lhs());
                let rhs = self.collect_expr_opt(e.rhs());
                let op = e.op_kind().map(BinaryOp::from);
                self.alloc_expr(Expr::BinaryOp { lhs, rhs, op }, syntax_ptr)
            }
            ast::Expr::TupleExpr(e) => {
                let exprs = e.exprs().map(|expr| self.collect_expr(expr)).collect();
                self.alloc_expr(Expr::Tuple { exprs }, syntax_ptr)
            }
            ast::Expr::BoxExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                self.alloc_expr(Expr::Box { expr }, syntax_ptr)
            }

            ast::Expr::ArrayExpr(e) => {
                let kind = e.kind();

                match kind {
                    ArrayExprKind::ElementList(e) => {
                        let exprs = e.map(|expr| self.collect_expr(expr)).collect();
                        self.alloc_expr(Expr::Array(Array::ElementList(exprs)), syntax_ptr)
                    }
                    ArrayExprKind::Repeat { initializer, repeat } => {
                        let initializer = self.collect_expr_opt(initializer);
                        let repeat = self.collect_expr_opt(repeat);
                        self.alloc_expr(
                            Expr::Array(Array::Repeat { initializer, repeat }),
                            syntax_ptr,
                        )
                    }
                }
            }

            ast::Expr::Literal(e) => self.alloc_expr(Expr::Literal(e.kind().into()), syntax_ptr),
            ast::Expr::IndexExpr(e) => {
                let base = self.collect_expr_opt(e.base());
                let index = self.collect_expr_opt(e.index());
                self.alloc_expr(Expr::Index { base, index }, syntax_ptr)
            }
            ast::Expr::RangeExpr(e) => {
                let lhs = e.start().map(|lhs| self.collect_expr(lhs));
                let rhs = e.end().map(|rhs| self.collect_expr(rhs));
                match e.op_kind() {
                    Some(range_type) => {
                        self.alloc_expr(Expr::Range { lhs, rhs, range_type }, syntax_ptr)
                    }
                    None => self.alloc_expr(Expr::Missing, syntax_ptr),
                }
            }
            ast::Expr::MacroCall(e) => {
                if let Some(name) = e.is_macro_rules().map(|it| it.as_name()) {
                    let mac = MacroDefId {
                        krate: Some(self.expander.module.krate),
                        ast_id: Some(self.expander.ast_id(&e)),
                        kind: MacroDefKind::Declarative,
                    };
                    self.body.item_scope.define_legacy_macro(name, mac);

                    // FIXME: do we still need to allocate this as missing ?
                    self.alloc_expr(Expr::Missing, syntax_ptr)
                } else {
                    let macro_call = self.expander.to_source(AstPtr::new(&e));
                    match self.expander.enter_expand(self.db, Some(&self.body.item_scope), e) {
                        Some((mark, expansion)) => {
                            self.source_map
                                .expansions
                                .insert(macro_call, self.expander.current_file_id);
                            let id = self.collect_expr(expansion);
                            self.expander.exit(self.db, mark);
                            id
                        }
                        None => self.alloc_expr(Expr::Missing, syntax_ptr),
                    }
                }
            }

            // FIXME implement HIR for these:
            ast::Expr::Label(_e) => self.alloc_expr(Expr::Missing, syntax_ptr),
        }
    }

    fn collect_expr_opt(&mut self, expr: Option<ast::Expr>) -> ExprId {
        if let Some(expr) = expr {
            self.collect_expr(expr)
        } else {
            self.missing_expr()
        }
    }

    fn collect_block(&mut self, expr: ast::BlockExpr) -> ExprId {
        let syntax_node_ptr = AstPtr::new(&expr.clone().into());
        let block = match expr.block() {
            Some(block) => block,
            None => return self.alloc_expr(Expr::Missing, syntax_node_ptr),
        };
        self.collect_block_items(&block);
        let statements = block
            .statements()
            .filter_map(|s| match s {
                ast::Stmt::LetStmt(stmt) => {
                    let pat = self.collect_pat_opt(stmt.pat());
                    let type_ref = stmt.ascribed_type().map(TypeRef::from_ast);
                    let initializer = stmt.initializer().map(|e| self.collect_expr(e));
                    Some(Statement::Let { pat, type_ref, initializer })
                }
                ast::Stmt::ExprStmt(stmt) => {
                    Some(Statement::Expr(self.collect_expr_opt(stmt.expr())))
                }
            })
            .collect();
        let tail = block.expr().map(|e| self.collect_expr(e));
        self.alloc_expr(Expr::Block { statements, tail }, syntax_node_ptr)
    }

    fn collect_block_items(&mut self, block: &ast::Block) {
        let container = ContainerId::DefWithBodyId(self.def);
        for item in block.items() {
            let (def, name): (ModuleDefId, Option<ast::Name>) = match item {
                ast::ModuleItem::FnDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (
                        FunctionLoc { container: container.into(), ast_id }.intern(self.db).into(),
                        def.name(),
                    )
                }
                ast::ModuleItem::TypeAliasDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (
                        TypeAliasLoc { container: container.into(), ast_id }.intern(self.db).into(),
                        def.name(),
                    )
                }
                ast::ModuleItem::ConstDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (
                        ConstLoc { container: container.into(), ast_id }.intern(self.db).into(),
                        def.name(),
                    )
                }
                ast::ModuleItem::StaticDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (StaticLoc { container, ast_id }.intern(self.db).into(), def.name())
                }
                ast::ModuleItem::StructDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (StructLoc { container, ast_id }.intern(self.db).into(), def.name())
                }
                ast::ModuleItem::EnumDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (EnumLoc { container, ast_id }.intern(self.db).into(), def.name())
                }
                ast::ModuleItem::UnionDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (UnionLoc { container, ast_id }.intern(self.db).into(), def.name())
                }
                ast::ModuleItem::TraitDef(def) => {
                    let ast_id = self.expander.ast_id(&def);
                    (TraitLoc { container, ast_id }.intern(self.db).into(), def.name())
                }
                ast::ModuleItem::ExternBlock(_) => continue, // FIXME: collect from extern blocks
                ast::ModuleItem::ImplDef(_)
                | ast::ModuleItem::UseItem(_)
                | ast::ModuleItem::ExternCrateItem(_)
                | ast::ModuleItem::Module(_)
                | ast::ModuleItem::MacroCall(_) => continue,
            };
            self.body.item_scope.define_def(def);
            if let Some(name) = name {
                let vis = crate::visibility::Visibility::Public; // FIXME determine correctly
                self.body
                    .item_scope
                    .push_res(name.as_name(), crate::per_ns::PerNs::from_def(def, vis));
            }
        }
    }

    fn collect_block_opt(&mut self, expr: Option<ast::BlockExpr>) -> ExprId {
        if let Some(block) = expr {
            self.collect_block(block)
        } else {
            self.missing_expr()
        }
    }

    fn collect_pat(&mut self, pat: ast::Pat) -> PatId {
        let pattern = match &pat {
            ast::Pat::BindPat(bp) => {
                let name = bp.name().map(|nr| nr.as_name()).unwrap_or_else(Name::missing);
                let annotation =
                    BindingAnnotation::new(bp.mut_token().is_some(), bp.ref_token().is_some());
                let subpat = bp.pat().map(|subpat| self.collect_pat(subpat));
                if annotation == BindingAnnotation::Unannotated && subpat.is_none() {
                    // This could also be a single-segment path pattern. To
                    // decide that, we need to try resolving the name.
                    let (resolved, _) = self.expander.crate_def_map.resolve_path(
                        self.db,
                        self.expander.module.local_id,
                        &name.clone().into(),
                        BuiltinShadowMode::Other,
                    );
                    match resolved.take_values() {
                        Some(ModuleDefId::ConstId(_)) => Pat::Path(name.into()),
                        Some(ModuleDefId::EnumVariantId(_)) => {
                            // this is only really valid for unit variants, but
                            // shadowing other enum variants with a pattern is
                            // an error anyway
                            Pat::Path(name.into())
                        }
                        Some(ModuleDefId::AdtId(AdtId::StructId(s)))
                            if self.db.struct_data(s).variant_data.kind() != StructKind::Record =>
                        {
                            // Funnily enough, record structs *can* be shadowed
                            // by pattern bindings (but unit or tuple structs
                            // can't).
                            Pat::Path(name.into())
                        }
                        // shadowing statics is an error as well, so we just ignore that case here
                        _ => Pat::Bind { name, mode: annotation, subpat },
                    }
                } else {
                    Pat::Bind { name, mode: annotation, subpat }
                }
            }
            ast::Pat::TupleStructPat(p) => {
                let path = p.path().and_then(|path| self.expander.parse_path(path));
                let args = p.args().map(|p| self.collect_pat(p)).collect();
                Pat::TupleStruct { path, args }
            }
            ast::Pat::RefPat(p) => {
                let pat = self.collect_pat_opt(p.pat());
                let mutability = Mutability::from_mutable(p.mut_token().is_some());
                Pat::Ref { pat, mutability }
            }
            ast::Pat::PathPat(p) => {
                let path = p.path().and_then(|path| self.expander.parse_path(path));
                path.map(Pat::Path).unwrap_or(Pat::Missing)
            }
            ast::Pat::OrPat(p) => {
                let pats = p.pats().map(|p| self.collect_pat(p)).collect();
                Pat::Or(pats)
            }
            ast::Pat::ParenPat(p) => return self.collect_pat_opt(p.pat()),
            ast::Pat::TuplePat(p) => {
                let args = p.args().map(|p| self.collect_pat(p)).collect();
                Pat::Tuple(args)
            }
            ast::Pat::PlaceholderPat(_) | ast::Pat::DotDotPat(_) => Pat::Wild,
            ast::Pat::RecordPat(p) => {
                let path = p.path().and_then(|path| self.expander.parse_path(path));
                let record_field_pat_list =
                    p.record_field_pat_list().expect("every struct should have a field list");
                let mut fields: Vec<_> = record_field_pat_list
                    .bind_pats()
                    .filter_map(|bind_pat| {
                        let ast_pat =
                            ast::Pat::cast(bind_pat.syntax().clone()).expect("bind pat is a pat");
                        let pat = self.collect_pat(ast_pat);
                        let name = bind_pat.name()?.as_name();
                        Some(RecordFieldPat { name, pat })
                    })
                    .collect();
                let iter = record_field_pat_list.record_field_pats().filter_map(|f| {
                    let ast_pat = f.pat()?;
                    let pat = self.collect_pat(ast_pat);
                    let name = f.name()?.as_name();
                    Some(RecordFieldPat { name, pat })
                });
                fields.extend(iter);

                Pat::Record { path, args: fields }
            }
            ast::Pat::SlicePat(p) => {
                let SlicePatComponents { prefix, slice, suffix } = p.components();

                Pat::Slice {
                    prefix: prefix.into_iter().map(|p| self.collect_pat(p)).collect(),
                    slice: slice.map(|p| self.collect_pat(p)),
                    suffix: suffix.into_iter().map(|p| self.collect_pat(p)).collect(),
                }
            }
            ast::Pat::LiteralPat(lit) => {
                if let Some(ast_lit) = lit.literal() {
                    let expr = Expr::Literal(ast_lit.kind().into());
                    let expr_ptr = AstPtr::new(&ast::Expr::Literal(ast_lit));
                    let expr_id = self.alloc_expr(expr, expr_ptr);
                    Pat::Lit(expr_id)
                } else {
                    Pat::Missing
                }
            }

            // FIXME: implement
            ast::Pat::BoxPat(_) | ast::Pat::RangePat(_) | ast::Pat::MacroPat(_) => Pat::Missing,
        };
        let ptr = AstPtr::new(&pat);
        self.alloc_pat(pattern, Either::Left(ptr))
    }

    fn collect_pat_opt(&mut self, pat: Option<ast::Pat>) -> PatId {
        if let Some(pat) = pat {
            self.collect_pat(pat)
        } else {
            self.missing_pat()
        }
    }
}

impl From<ast::BinOp> for BinaryOp {
    fn from(ast_op: ast::BinOp) -> Self {
        match ast_op {
            ast::BinOp::BooleanOr => BinaryOp::LogicOp(LogicOp::Or),
            ast::BinOp::BooleanAnd => BinaryOp::LogicOp(LogicOp::And),
            ast::BinOp::EqualityTest => BinaryOp::CmpOp(CmpOp::Eq { negated: false }),
            ast::BinOp::NegatedEqualityTest => BinaryOp::CmpOp(CmpOp::Eq { negated: true }),
            ast::BinOp::LesserEqualTest => {
                BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less, strict: false })
            }
            ast::BinOp::GreaterEqualTest => {
                BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: false })
            }
            ast::BinOp::LesserTest => {
                BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less, strict: true })
            }
            ast::BinOp::GreaterTest => {
                BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: true })
            }
            ast::BinOp::Addition => BinaryOp::ArithOp(ArithOp::Add),
            ast::BinOp::Multiplication => BinaryOp::ArithOp(ArithOp::Mul),
            ast::BinOp::Subtraction => BinaryOp::ArithOp(ArithOp::Sub),
            ast::BinOp::Division => BinaryOp::ArithOp(ArithOp::Div),
            ast::BinOp::Remainder => BinaryOp::ArithOp(ArithOp::Rem),
            ast::BinOp::LeftShift => BinaryOp::ArithOp(ArithOp::Shl),
            ast::BinOp::RightShift => BinaryOp::ArithOp(ArithOp::Shr),
            ast::BinOp::BitwiseXor => BinaryOp::ArithOp(ArithOp::BitXor),
            ast::BinOp::BitwiseOr => BinaryOp::ArithOp(ArithOp::BitOr),
            ast::BinOp::BitwiseAnd => BinaryOp::ArithOp(ArithOp::BitAnd),
            ast::BinOp::Assignment => BinaryOp::Assignment { op: None },
            ast::BinOp::AddAssign => BinaryOp::Assignment { op: Some(ArithOp::Add) },
            ast::BinOp::DivAssign => BinaryOp::Assignment { op: Some(ArithOp::Div) },
            ast::BinOp::MulAssign => BinaryOp::Assignment { op: Some(ArithOp::Mul) },
            ast::BinOp::RemAssign => BinaryOp::Assignment { op: Some(ArithOp::Rem) },
            ast::BinOp::ShlAssign => BinaryOp::Assignment { op: Some(ArithOp::Shl) },
            ast::BinOp::ShrAssign => BinaryOp::Assignment { op: Some(ArithOp::Shr) },
            ast::BinOp::SubAssign => BinaryOp::Assignment { op: Some(ArithOp::Sub) },
            ast::BinOp::BitOrAssign => BinaryOp::Assignment { op: Some(ArithOp::BitOr) },
            ast::BinOp::BitAndAssign => BinaryOp::Assignment { op: Some(ArithOp::BitAnd) },
            ast::BinOp::BitXorAssign => BinaryOp::Assignment { op: Some(ArithOp::BitXor) },
        }
    }
}

impl From<ast::LiteralKind> for Literal {
    fn from(ast_lit_kind: ast::LiteralKind) -> Self {
        match ast_lit_kind {
            LiteralKind::IntNumber { suffix } => {
                let known_name = suffix.and_then(|it| BuiltinInt::from_suffix(&it));

                Literal::Int(Default::default(), known_name)
            }
            LiteralKind::FloatNumber { suffix } => {
                let known_name = suffix.and_then(|it| BuiltinFloat::from_suffix(&it));

                Literal::Float(Default::default(), known_name)
            }
            LiteralKind::ByteString => Literal::ByteString(Default::default()),
            LiteralKind::String => Literal::String(Default::default()),
            LiteralKind::Byte => Literal::Int(Default::default(), Some(BuiltinInt::U8)),
            LiteralKind::Bool(val) => Literal::Bool(val),
            LiteralKind::Char => Literal::Char(Default::default()),
        }
    }
}
