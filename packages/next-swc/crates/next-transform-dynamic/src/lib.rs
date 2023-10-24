// TODO(alexkirsz) Remove once the diagnostic is fixed.
#![allow(rustc::untranslatable_diagnostic_trivial)]

use std::path::{Path, PathBuf};

use pathdiff::diff_paths;
use swc_core::{
    common::{errors::HANDLER, FileName, Span, DUMMY_SP},
    ecma::{
        ast::{
            op, ArrayLit, ArrowExpr, BinExpr, BinaryOp, BlockStmt, BlockStmtOrExpr, Bool, CallExpr,
            Callee, Expr, ExprOrSpread, ExprStmt, Id, Ident, ImportDecl, ImportDefaultSpecifier,
            ImportNamedSpecifier, ImportSpecifier, KeyValueProp, Lit, Module, ModuleDecl,
            ModuleItem, Null, ObjectLit, Prop, PropName, PropOrSpread, Stmt, Str, Tpl, UnaryExpr,
            UnaryOp,
        },
        utils::{prepend_stmt, private_ident, quote_ident, ExprExt, ExprFactory},
        visit::{noop_visit_mut_type, Fold, FoldWith, VisitMut, VisitMutWith},
    },
    quote,
};

/// Creates a SWC visitor to transform `next/dynamic` calls to have the
/// corresponding `loadableGenerated` property.
///
/// [NOTE] We do not use `NextDynamicMode::Turbopack` yet. It isn't compatible
/// with current loadable manifest, which causes hydration errors.
pub fn next_dynamic(
    is_development: bool,
    is_server: bool,
    is_rsc_server_layer: bool,
    mode: NextDynamicMode,
    filename: FileName,
    pages_dir: Option<PathBuf>,
) -> impl Fold {
    NextDynamicPatcher {
        is_development,
        is_server,
        is_rsc_server_layer,
        pages_dir,
        filename,
        dynamic_bindings: vec![],
        is_next_dynamic_first_arg: false,
        dynamically_imported_specifier: None,
        added_nextjs_pure_import: false,
        state: match mode {
            NextDynamicMode::Webpack => NextDynamicPatcherState::Webpack,
            NextDynamicMode::Turbopack {
                dynamic_transition_name,
            } => NextDynamicPatcherState::Turbopack {
                dynamic_transition_name,
                imports: vec![],
            },
        },
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum NextDynamicMode {
    /// In Webpack mode, each `dynamic()` call will generate a key composed
    /// from:
    /// 1. The current module's path relative to the pages directory;
    /// 2. The relative imported module id.
    ///
    /// This key is of the form:
    /// {currentModulePath} -> {relativeImportedModulePath}
    ///
    /// It corresponds to an entry in the React Loadable Manifest generated by
    /// the React Loadable Webpack plugin.
    Webpack,
    /// In Turbopack mode:
    /// * in development, each `dynamic()` call will generate a key containing
    ///   both the imported module id and the chunks it needs. This removes the
    ///   need for a manifest entry
    /// * during build, each `dynamic()` call will import the module through the
    ///   given transition, which takes care of adding an entry to the manifest
    ///   and returning an asset that exports the entry's key.
    Turbopack { dynamic_transition_name: String },
}

#[derive(Debug)]
struct NextDynamicPatcher {
    is_development: bool,
    is_server: bool,
    is_rsc_server_layer: bool,
    pages_dir: Option<PathBuf>,
    filename: FileName,
    dynamic_bindings: Vec<Id>,
    is_next_dynamic_first_arg: bool,
    dynamically_imported_specifier: Option<(String, Span)>,
    state: NextDynamicPatcherState,
    added_nextjs_pure_import: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum NextDynamicPatcherState {
    Webpack,
    /// In Turbo mode, contains a list of modules that need to be imported with
    /// the given transition under a particular ident.
    #[allow(unused)]
    Turbopack {
        dynamic_transition_name: String,
        imports: Vec<TurbopackImport>,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum TurbopackImport {
    DevelopmentTransition {
        id_ident: Ident,
        chunks_ident: Ident,
        specifier: String,
    },
    DevelopmentId {
        id_ident: Ident,
        specifier: String,
    },
    BuildTransition {
        id_ident: Ident,
        specifier: String,
    },
    BuildId {
        id_ident: Ident,
        specifier: String,
    },
}

impl Fold for NextDynamicPatcher {
    fn fold_module(&mut self, mut m: Module) -> Module {
        m = m.fold_children_with(self);

        // dbg!(self.added_nextjs_pure_import);
        // println!("detect self.added_nextjs_pure_import");
        if self.added_nextjs_pure_import {
            let import_expression = quote!(
                "import { __nextjs_pure } from 'next/dist/build/swc/helpers';" as ModuleItem
            );
            prepend_stmt(&mut m.body, import_expression);
        }
        m
    }

    fn fold_module_items(&mut self, mut items: Vec<ModuleItem>) -> Vec<ModuleItem> {
        items = items.fold_children_with(self);

        self.maybe_add_dynamically_imported_specifier(&mut items);

        items
    }

    fn fold_import_decl(&mut self, decl: ImportDecl) -> ImportDecl {
        let ImportDecl {
            ref src,
            ref specifiers,
            ..
        } = decl;
        if &src.value == "next/dynamic" {
            for specifier in specifiers {
                if let ImportSpecifier::Default(default_specifier) = specifier {
                    self.dynamic_bindings.push(default_specifier.local.to_id());
                }
            }
        }

        decl
    }

    fn fold_call_expr(&mut self, expr: CallExpr) -> CallExpr {
        if self.is_next_dynamic_first_arg {
            if let Callee::Import(..) = &expr.callee {
                match &*expr.args[0].expr {
                    Expr::Lit(Lit::Str(Str { value, span, .. })) => {
                        self.dynamically_imported_specifier = Some((value.to_string(), *span));
                    }
                    Expr::Tpl(Tpl { exprs, quasis, .. }) if exprs.is_empty() => {
                        self.dynamically_imported_specifier =
                            Some((quasis[0].raw.to_string(), quasis[0].span));
                    }
                    _ => {}
                }
            }
            return expr.fold_children_with(self);
        }
        let mut expr = expr.fold_children_with(self);
        if let Callee::Expr(i) = &expr.callee {
            if let Expr::Ident(identifier) = &**i {
                if self.dynamic_bindings.contains(&identifier.to_id()) {
                    if expr.args.is_empty() {
                        HANDLER.with(|handler| {
                            handler
                                .struct_span_err(
                                    identifier.span,
                                    "next/dynamic requires at least one argument",
                                )
                                .emit()
                        });
                        return expr;
                    } else if expr.args.len() > 2 {
                        HANDLER.with(|handler| {
                            handler
                                .struct_span_err(
                                    identifier.span,
                                    "next/dynamic only accepts 2 arguments",
                                )
                                .emit()
                        });
                        return expr;
                    }
                    if expr.args.len() == 2 {
                        match &*expr.args[1].expr {
                            Expr::Object(_) => {}
                            _ => {
                                HANDLER.with(|handler| {
                          handler
                              .struct_span_err(
                                  identifier.span,
                                  "next/dynamic options must be an object literal.\nRead more: https://nextjs.org/docs/messages/invalid-dynamic-options-type",
                              )
                              .emit();
                      });
                                return expr;
                            }
                        }
                    }

                    self.is_next_dynamic_first_arg = true;
                    expr.args[0].expr = expr.args[0].expr.clone().fold_with(self);
                    self.is_next_dynamic_first_arg = false;

                    let Some((dynamically_imported_specifier, dynamically_imported_specifier_span)) =
                        self.dynamically_imported_specifier.take()
                    else {
                        return expr;
                    };

                    // dev client or server:
                    // loadableGenerated: {
                    //   modules:
                    // ["/project/src/file-being-transformed.js -> " + '../components/hello'] }

                    // prod client
                    // loadableGenerated: {
                    //   webpack: () => [require.resolveWeak('../components/hello')],
                    let generated = Box::new(Expr::Object(ObjectLit {
                        span: DUMMY_SP,
                        props: match &mut self.state {
                            NextDynamicPatcherState::Webpack => {
                                if self.is_development || self.is_server {
                                    module_id_options(quote!(
                                        "$left + $right" as Expr,
                                        left: Expr = format!(
                                            "{} -> ",
                                            rel_filename(self.pages_dir.as_deref(), &self.filename)
                                        )
                                        .into(),
                                        right: Expr = dynamically_imported_specifier.into(),
                                    ))
                                } else {
                                    webpack_options(quote!(
                                        "require.resolveWeak($id)" as Expr,
                                        id: Expr = dynamically_imported_specifier.into()
                                    ))
                                }
                            }
                            NextDynamicPatcherState::Turbopack { imports, .. } => {
                                let id_ident =
                                    private_ident!(dynamically_imported_specifier_span, "id");

                                match (self.is_development, self.is_server) {
                                    (true, true) => {
                                        let chunks_ident = private_ident!(
                                            dynamically_imported_specifier_span,
                                            "chunks"
                                        );

                                        imports.push(TurbopackImport::DevelopmentTransition {
                                            id_ident: id_ident.clone(),
                                            chunks_ident: chunks_ident.clone(),
                                            specifier: dynamically_imported_specifier,
                                        });

                                        // On the server, the key needs to be serialized because it
                                        // will be used to index the React Loadable Manifest, which
                                        // is a normal JS object. In Turbo mode, this is a proxy,
                                        // but the key will still be coerced to a string.
                                        module_id_options(quote!(
                                            r#"
                                            JSON.stringify({
                                                id: $id,
                                                chunks: $chunks
                                            })
                                            "# as Expr,
                                            id = id_ident,
                                            chunks = chunks_ident,
                                        ))
                                    }
                                    (true, false) => {
                                        imports.push(TurbopackImport::DevelopmentId {
                                            id_ident: id_ident.clone(),
                                            specifier: dynamically_imported_specifier,
                                        });

                                        // On the client, we only need the target module ID, which
                                        // will be reported under the `dynamicIds` property of Next
                                        // data.
                                        module_id_options(Expr::Ident(id_ident))
                                    }
                                    (false, true) => {
                                        let id_ident = private_ident!(
                                            dynamically_imported_specifier_span,
                                            "id"
                                        );

                                        imports.push(TurbopackImport::BuildTransition {
                                            id_ident: id_ident.clone(),
                                            specifier: dynamically_imported_specifier.clone(),
                                        });

                                        module_id_options(Expr::Ident(id_ident))
                                    }
                                    (false, false) => {
                                        let id_ident = private_ident!(
                                            dynamically_imported_specifier_span,
                                            "id"
                                        );

                                        imports.push(TurbopackImport::BuildId {
                                            id_ident: id_ident.clone(),
                                            specifier: dynamically_imported_specifier.clone(),
                                        });

                                        module_id_options(Expr::Ident(id_ident))
                                    }
                                }
                            }
                        },
                    }));

                    let mut props =
                        vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                            key: PropName::Ident(Ident::new("loadableGenerated".into(), DUMMY_SP)),
                            value: generated,
                        })))];

                    let mut has_ssr_false = false;

                    if expr.args.len() == 2 {
                        if let Expr::Object(ObjectLit {
                            props: options_props,
                            ..
                        }) = &*expr.args[1].expr
                        {
                            for prop in options_props.iter() {
                                if let Some(KeyValueProp { key, value }) = match prop {
                                    PropOrSpread::Prop(prop) => match &**prop {
                                        Prop::KeyValue(key_value_prop) => Some(key_value_prop),
                                        _ => None,
                                    },
                                    _ => None,
                                } {
                                    if let Some(Ident {
                                        sym,
                                        span: _,
                                        optional: _,
                                    }) = match key {
                                        PropName::Ident(ident) => Some(ident),
                                        _ => None,
                                    } {
                                        if sym == "ssr" {
                                            if let Some(Lit::Bool(Bool {
                                                value: false,
                                                span: _,
                                            })) = value.as_lit()
                                            {
                                                has_ssr_false = true
                                            }
                                        }
                                        // if sym == "suspense" {
                                        //     if let Some(Lit::Bool(Bool {
                                        //         value: true,
                                        //         span: _,
                                        //     })) = value.as_lit()
                                        //     {
                                        //         has_suspense = true
                                        //     }
                                        // }
                                    }
                                }
                            }
                            props.extend(options_props.iter().cloned());
                        }
                    }

                    // Don't strip the `loader` argument if suspense is true
                    // See https://github.com/vercel/next.js/issues/36636 for background.

                    // Also don't strip the `loader` argument for server components (both
                    // server/client layers), since they're aliased to a
                    // React.lazy implementation.
                    // if has_ssr_false
                    //     && !has_suspense
                    //     && self.is_server
                    //     && !self.is_server_components
                    // {
                    //     expr.args[0] = Lit::Null(Null { span: DUMMY_SP }).as_arg();
                    // }

                    if has_ssr_false && self.is_server {
                        // if it's server components SSR layer
                        if !self.is_rsc_server_layer {
                            // Transform 1st argument `expr.args[0]` aka the module loader to:
                            // (() => {
                            //    expr.args[0]
                            // })`
                            // For instance:
                            // dynamic((() => {
                            //   /**
                            //    * this will make sure we can traverse the module first but will be
                            //    * tree-shake out in server bundle */
                            //   __next_pure(() => import('./client-mod'))
                            // }), { ssr: false })

                            self.added_nextjs_pure_import = true;

                            // create function call of `__next_js` wrapping the
                            // `side_effect_free_loader_arg.as_arg()`
                            let pure_fn_call = Expr::Call(CallExpr {
                                span: DUMMY_SP,
                                callee: quote_ident!("__nextjs_pure").as_callee(),
                                args: vec![expr.args[0].expr.clone().as_arg()],
                                type_args: Default::default(),
                            });

                            let side_effect_free_loader_arg = Expr::Arrow(ArrowExpr {
                                span: DUMMY_SP,
                                params: vec![],
                                body: Box::new(BlockStmtOrExpr::BlockStmt(BlockStmt {
                                    span: DUMMY_SP,
                                    stmts: vec![
                                        // loader is still inside the module but not executed,
                                        // then it will be removed by tree-shaking.
                                        Stmt::Expr(ExprStmt {
                                            span: DUMMY_SP,
                                            expr: Box::new(pure_fn_call), /* expr.args[0].expr.
                                                                           * clone(), */
                                        }),
                                    ],
                                })),
                                is_async: true,
                                is_generator: false,
                                type_params: None,
                                return_type: None,
                            });

                            expr.args[0] = side_effect_free_loader_arg.as_arg();
                        }
                        // else {
                        //     expr.args[0] = Lit::Null(Null { span: DUMMY_SP
                        // }).as_arg(); }
                    }

                    let second_arg = ExprOrSpread {
                        spread: None,
                        expr: Box::new(Expr::Object(ObjectLit {
                            span: DUMMY_SP,
                            props,
                        })),
                    };

                    if expr.args.len() == 2 {
                        expr.args[1] = second_arg;
                    } else {
                        expr.args.push(second_arg)
                    }
                }
            }
        }
        expr
    }
}

fn module_id_options(module_id: Expr) -> Vec<PropOrSpread> {
    vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
        key: PropName::Ident(Ident::new("modules".into(), DUMMY_SP)),
        value: Box::new(Expr::Array(ArrayLit {
            elems: vec![Some(ExprOrSpread {
                expr: Box::new(module_id),
                spread: None,
            })],
            span: DUMMY_SP,
        })),
    })))]
}

fn webpack_options(module_id: Expr) -> Vec<PropOrSpread> {
    vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
        key: PropName::Ident(Ident::new("webpack".into(), DUMMY_SP)),
        value: Box::new(Expr::Arrow(ArrowExpr {
            params: vec![],
            body: Box::new(BlockStmtOrExpr::Expr(Box::new(Expr::Array(ArrayLit {
                elems: vec![Some(ExprOrSpread {
                    expr: Box::new(module_id),
                    spread: None,
                })],
                span: DUMMY_SP,
            })))),
            is_async: false,
            is_generator: false,
            span: DUMMY_SP,
            return_type: None,
            type_params: None,
        })),
    })))]
}

impl NextDynamicPatcher {
    fn maybe_add_dynamically_imported_specifier(&mut self, items: &mut Vec<ModuleItem>) {
        let NextDynamicPatcherState::Turbopack {
            dynamic_transition_name,
            imports,
        } = &mut self.state
        else {
            return;
        };

        let mut new_items = Vec::with_capacity(imports.len() * 2);

        for import in std::mem::take(imports) {
            match import {
                TurbopackImport::DevelopmentTransition {
                    id_ident,
                    chunks_ident,
                    specifier,
                } => {
                    // The transition should return both the target module's id
                    // and the chunks it needs to run.
                    new_items.push(ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                        span: DUMMY_SP,
                        expr: Box::new(Expr::Lit(Lit::Str(
                            format!("TURBOPACK {{ transition: {dynamic_transition_name} }}").into(),
                        ))),
                    })));
                    new_items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                        span: DUMMY_SP,
                        specifiers: vec![
                            ImportSpecifier::Default(ImportDefaultSpecifier {
                                span: DUMMY_SP,
                                local: id_ident,
                            }),
                            ImportSpecifier::Named(ImportNamedSpecifier {
                                span: DUMMY_SP,
                                local: chunks_ident,
                                imported: Some(Ident::new("chunks".into(), DUMMY_SP).into()),
                                is_type_only: false,
                            }),
                        ],
                        src: Box::new(specifier.into()),
                        type_only: false,
                        with: None,
                    })));
                }
                TurbopackImport::DevelopmentId {
                    id_ident,
                    specifier,
                } => {
                    // We don't want this import to cause the imported module to be considered for
                    // chunking through this import; we only need the module id.
                    new_items.push(quote!(
                        "\"TURBOPACK { chunking-type: none }\";" as ModuleItem
                    ));
                    // Turbopack will automatically transform the imported `__turbopack_module_id__`
                    // identifier into the imported module's id.
                    new_items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                        span: DUMMY_SP,
                        specifiers: vec![ImportSpecifier::Named(ImportNamedSpecifier {
                            span: DUMMY_SP,
                            local: id_ident,
                            imported: Some(
                                Ident::new("__turbopack_module_id__".into(), DUMMY_SP).into(),
                            ),
                            is_type_only: false,
                        })],
                        src: Box::new(specifier.into()),
                        type_only: false,
                        with: None,
                    })));
                }
                TurbopackImport::BuildTransition {
                    id_ident,
                    specifier,
                } => {
                    // The transition should make sure the imported module ends up in the dynamic
                    // manifest.
                    new_items.push(ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                        span: DUMMY_SP,
                        expr: Box::new(Expr::Lit(Lit::Str(
                            format!("TURBOPACK {{ transition: {dynamic_transition_name} }}").into(),
                        ))),
                    })));
                    // Turbopack will automatically transform the imported `__turbopack_module_id__`
                    // identifier into the imported module's id.
                    new_items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                        span: DUMMY_SP,
                        specifiers: vec![ImportSpecifier::Named(ImportNamedSpecifier {
                            span: DUMMY_SP,
                            local: id_ident,
                            imported: Some(
                                Ident::new("__turbopack_module_id__".into(), DUMMY_SP).into(),
                            ),
                            is_type_only: false,
                        })],
                        src: Box::new(specifier.into()),
                        type_only: false,
                        with: None,
                    })));
                }
                TurbopackImport::BuildId {
                    id_ident,
                    specifier,
                } => {
                    // We don't want this import to cause the imported module to be considered for
                    // chunking through this import; we only need the module id.
                    new_items.push(quote!(
                        "\"TURBOPACK { chunking-type: none }\";" as ModuleItem
                    ));
                    // Turbopack will automatically transform the imported `__turbopack_module_id__`
                    // identifier into the imported module's id.
                    new_items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                        span: DUMMY_SP,
                        specifiers: vec![ImportSpecifier::Named(ImportNamedSpecifier {
                            span: DUMMY_SP,
                            local: id_ident,
                            imported: Some(
                                Ident::new("__turbopack_module_id__".into(), DUMMY_SP).into(),
                            ),
                            is_type_only: false,
                        })],
                        src: Box::new(specifier.into()),
                        type_only: false,
                        with: None,
                    })));
                }
            }
        }

        new_items.append(items);

        std::mem::swap(&mut new_items, items)
    }
}

// Receive an expression and return `typeof window !== 'undefined' &&
// <expression>`, to make the expression is tree-shakable on server side but
// still remain in module graph.
fn wrap_expr_with_client_only_cond(wrapped_expr: &Expr) -> Expr {
    let typeof_expr = Expr::Unary(UnaryExpr {
        span: DUMMY_SP,
        op: UnaryOp::TypeOf, // 'typeof' operator
        arg: Box::new(Expr::Ident(Ident {
            span: DUMMY_SP,
            sym: "window".into(),
            optional: false,
        })),
    });
    let undefined_literal = Expr::Lit(Lit::Str(Str {
        span: DUMMY_SP,
        value: "undefined".into(),
        raw: None,
    }));
    let inequality_expr = Expr::Bin(BinExpr {
        span: DUMMY_SP,
        left: Box::new(typeof_expr),
        op: BinaryOp::NotEq, // '!=='
        right: Box::new(undefined_literal),
    });

    // Create the LogicalExpr 'typeof window !== "undefined" && x'
    let logical_expr = Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: op!("&&"), // '&&' operator
        left: Box::new(inequality_expr),
        right: Box::new(wrapped_expr.clone()),
    });

    logical_expr
}

fn rel_filename(base: Option<&Path>, file: &FileName) -> String {
    let base = match base {
        Some(v) => v,
        None => return file.to_string(),
    };

    let file = match file {
        FileName::Real(v) => v,
        _ => {
            return file.to_string();
        }
    };

    let rel_path = diff_paths(file, base);

    let rel_path = match rel_path {
        Some(v) => v,
        None => return file.display().to_string(),
    };

    rel_path.display().to_string()
}
