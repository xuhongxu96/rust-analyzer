//! Completion of names from the current scope in type position.

use hir::{HirDisplay, ModuleDef, PathResolution, ScopeDef};
use ide_db::FxHashSet;
use syntax::{ast, AstNode};

use crate::{
    context::{PathCompletionCtx, PathKind, PathQualifierCtx},
    patterns::{ImmediateLocation, TypeAnnotation},
    render::render_type_inference,
    CompletionContext, Completions,
};

pub(crate) fn complete_type_path(acc: &mut Completions, ctx: &CompletionContext) {
    let _p = profile::span("complete_type_path");

    let (&is_absolute_path, qualifier) = match ctx.path_context() {
        Some(PathCompletionCtx {
            kind: PathKind::Type { .. },
            is_absolute_path,
            qualifier,
            ..
        }) => (is_absolute_path, qualifier),
        _ => return,
    };

    let scope_def_applicable = |def| {
        use hir::{GenericParam::*, ModuleDef::*};
        match def {
            ScopeDef::GenericParam(LifetimeParam(_)) | ScopeDef::Label(_) => false,
            // no values in type places
            ScopeDef::ModuleDef(Function(_) | Variant(_) | Static(_)) | ScopeDef::Local(_) => false,
            // unless its a constant in a generic arg list position
            ScopeDef::ModuleDef(Const(_)) | ScopeDef::GenericParam(ConstParam(_)) => {
                ctx.expects_generic_arg()
            }
            ScopeDef::ImplSelfType(_) => {
                !ctx.previous_token_is(syntax::T![impl]) && !ctx.previous_token_is(syntax::T![for])
            }
            // Don't suggest attribute macros and derives.
            ScopeDef::ModuleDef(Macro(mac)) => mac.is_fn_like(ctx.db),
            // Type things are fine
            ScopeDef::ModuleDef(BuiltinType(_) | Adt(_) | Module(_) | Trait(_) | TypeAlias(_))
            | ScopeDef::AdtSelfType(_)
            | ScopeDef::Unknown
            | ScopeDef::GenericParam(TypeParam(_)) => true,
        }
    };

    match qualifier {
        Some(PathQualifierCtx { is_infer_qualifier, resolution, .. }) => {
            if *is_infer_qualifier {
                ctx.traits_in_scope()
                    .0
                    .into_iter()
                    .flat_map(|it| hir::Trait::from(it).items(ctx.sema.db))
                    .for_each(|item| add_assoc_item(acc, ctx, item));
                return;
            }
            let resolution = match resolution {
                Some(it) => it,
                None => return,
            };
            // Add associated types on type parameters and `Self`.
            ctx.scope.assoc_type_shorthand_candidates(resolution, |_, alias| {
                acc.add_type_alias(ctx, alias);
                None::<()>
            });

            match resolution {
                hir::PathResolution::Def(hir::ModuleDef::Module(module)) => {
                    let module_scope = module.scope(ctx.db, Some(ctx.module));
                    for (name, def) in module_scope {
                        if scope_def_applicable(def) {
                            acc.add_resolution(ctx, name, def);
                        }
                    }
                }
                hir::PathResolution::Def(
                    def @ (hir::ModuleDef::Adt(_)
                    | hir::ModuleDef::TypeAlias(_)
                    | hir::ModuleDef::BuiltinType(_)),
                ) => {
                    let ty = match def {
                        hir::ModuleDef::Adt(adt) => adt.ty(ctx.db),
                        hir::ModuleDef::TypeAlias(a) => a.ty(ctx.db),
                        hir::ModuleDef::BuiltinType(builtin) => builtin.ty(ctx.db),
                        _ => unreachable!(),
                    };

                    // XXX: For parity with Rust bug #22519, this does not complete Ty::AssocType.
                    // (where AssocType is defined on a trait, not an inherent impl)

                    ty.iterate_path_candidates(
                        ctx.db,
                        &ctx.scope,
                        &ctx.traits_in_scope().0,
                        Some(ctx.module),
                        None,
                        |item| {
                            add_assoc_item(acc, ctx, item);
                            None::<()>
                        },
                    );

                    // Iterate assoc types separately
                    ty.iterate_assoc_items(ctx.db, ctx.krate, |item| {
                        if let hir::AssocItem::TypeAlias(ty) = item {
                            acc.add_type_alias(ctx, ty)
                        }
                        None::<()>
                    });
                }
                hir::PathResolution::Def(hir::ModuleDef::Trait(t)) => {
                    // Handles `Trait::assoc` as well as `<Ty as Trait>::assoc`.
                    for item in t.items(ctx.db) {
                        add_assoc_item(acc, ctx, item);
                    }
                }
                hir::PathResolution::TypeParam(_) | hir::PathResolution::SelfType(_) => {
                    let ty = match resolution {
                        hir::PathResolution::TypeParam(param) => param.ty(ctx.db),
                        hir::PathResolution::SelfType(impl_def) => impl_def.self_ty(ctx.db),
                        _ => return,
                    };

                    let mut seen = FxHashSet::default();
                    ty.iterate_path_candidates(
                        ctx.db,
                        &ctx.scope,
                        &ctx.traits_in_scope().0,
                        Some(ctx.module),
                        None,
                        |item| {
                            // We might iterate candidates of a trait multiple times here, so deduplicate
                            // them.
                            if seen.insert(item) {
                                add_assoc_item(acc, ctx, item);
                            }
                            None::<()>
                        },
                    );
                }
                _ => (),
            }
        }
        None if is_absolute_path => acc.add_crate_roots(ctx),
        None => {
            acc.add_nameref_keywords_with_colon(ctx);
            if let Some(ImmediateLocation::TypeBound) = &ctx.completion_location {
                ctx.process_all_names(&mut |name, res| {
                    let add_resolution = match res {
                        ScopeDef::ModuleDef(hir::ModuleDef::Macro(mac)) => mac.is_fn_like(ctx.db),
                        ScopeDef::ModuleDef(
                            hir::ModuleDef::Trait(_) | hir::ModuleDef::Module(_),
                        ) => true,
                        _ => false,
                    };
                    if add_resolution {
                        acc.add_resolution(ctx, name, res);
                    }
                });
                return;
            }
            if let Some(ImmediateLocation::GenericArgList(arg_list)) = &ctx.completion_location {
                if let Some(path_seg) = arg_list.syntax().parent().and_then(ast::PathSegment::cast)
                {
                    if path_seg.syntax().ancestors().find_map(ast::TypeBound::cast).is_some() {
                        if let Some(hir::PathResolution::Def(hir::ModuleDef::Trait(trait_))) =
                            ctx.sema.resolve_path(&path_seg.parent_path())
                        {
                            trait_.items_with_supertraits(ctx.sema.db).into_iter().for_each(|it| {
                                if let hir::AssocItem::TypeAlias(alias) = it {
                                    cov_mark::hit!(complete_assoc_type_in_generics_list);
                                    acc.add_type_alias_with_eq(ctx, alias)
                                }
                            });
                        }
                    }
                }
            }
            ctx.process_all_names(&mut |name, def| {
                if scope_def_applicable(def) {
                    acc.add_resolution(ctx, name, def);
                }
            });
        }
    }
}

pub(crate) fn complete_inferred_type(acc: &mut Completions, ctx: &CompletionContext) -> Option<()> {
    let path_qualifier = if let Some(path_ctx) = ctx.path_context() {
        // Do not infer type in absolute path
        if path_ctx.is_absolute_path {
            return None;
        }
        path_ctx.qualifier.as_ref()
    } else {
        None
    };

    use TypeAnnotation::*;
    let pat = match &ctx.completion_location {
        Some(ImmediateLocation::TypeAnnotation(t)) => t,
        _ => return None,
    };
    let x = match pat {
        Let(pat) | FnParam(pat) => ctx.sema.type_of_pat(pat.as_ref()?),
        Const(exp) | RetType(exp) => ctx.sema.type_of_expr(exp.as_ref()?),
    }?
    .adjusted();

    let qualified_module = match path_qualifier.and_then(|q| q.resolution.as_ref()) {
        // If a path qualifier is present, check if the type is an ADT in the same module path.
        // Drop the inferred type if it is not an ADT or in a different module path.
        Some(PathResolution::Def(ModuleDef::Module(module))) => {
            if x.as_adt()?.module(ctx.db) != *module {
                return None;
            }
            *module
        }
        _ => ctx.module,
    };

    let ty_string = x.display_source_code(ctx.db, qualified_module.into()).ok()?;
    acc.add(render_type_inference(ty_string, ctx));
    None
}

fn add_assoc_item(acc: &mut Completions, ctx: &CompletionContext, item: hir::AssocItem) {
    match item {
        hir::AssocItem::Const(ct) if ctx.expects_generic_arg() => acc.add_const(ctx, ct),
        hir::AssocItem::Function(_) | hir::AssocItem::Const(_) => (),
        hir::AssocItem::TypeAlias(ty) => acc.add_type_alias(ctx, ty),
    }
}

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};

    use crate::tests::completion_list_no_kw;

    fn check(ra_fixture: &str, expect: Expect) {
        let actual = completion_list_no_kw(ra_fixture);
        expect.assert_eq(&actual);
    }

    #[test]
    fn does_not_infer_type_in_absolute_path() {
        check(
            r#"
        fn f() -> ::$0 { }
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn completes_inferred_type() {
        check(
            r#"
        mod m { pub struct MyStruct {} }
        use m::MyStruct;
        fn f() -> My$0 { MyStruct {} }
"#,
            expect![[r#"
            md m
            st MyStruct
            bt u32
            it MyStruct
        "#]],
        );
    }

    #[test]
    fn does_not_complete_inferred_type_in_different_qualified_path() {
        check(
            r#"
        mod m { pub struct MyStruct {} }
        fn f() -> m::My$0 { 1 }
"#,
            expect![[r#"
            st MyStruct
        "#]],
        );
    }

    #[test]
    fn completes_inferred_type_in_same_qualified_path() {
        check(
            r#"
        mod m { pub struct MyStruct {} }
        fn f() -> m::My$0 { m::MyStruct {} }
"#,
            expect![[r#"
            st MyStruct
            it MyStruct
        "#]],
        );
    }
}
