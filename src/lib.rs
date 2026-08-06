extern crate proc_macro;
use once_cell::sync::Lazy;
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use std::collections::HashMap;
use std::sync::RwLock;
use syn::{parse_macro_input, ItemFn, LitStr};

const HTTP_VERBS: [&str; 5] = ["get", "post", "put", "delete", "patch"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteInfo {
    prefix: String,       // The scope or module key (e.g., "/events")
    handler_name: String, // The name of the handler function
    path: String,         // The route path (e.g., "/search")
    verb: String,         // The HTTP method (e.g., "GET")
}

type Registry = RwLock<HashMap<String, Vec<RouteInfo>>>;

// Use a global RwLock map for storing registrations per unique module key
static REGISTRATION_MAP: Lazy<Registry> = Lazy::new(|| RwLock::new(HashMap::new()));

fn is_http_verb_attr(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|segment| HTTP_VERBS.contains(&segment.ident.to_string().as_str()))
}

/// Extracts the route path and HTTP verb, registers the route under `prefix`, and strips the
/// Actix route attribute (e.g. `#[get("/path")]`) from the emitted function.
///
/// The attribute must be stripped: Actix's own `#[get]`/`#[post]` macros turn the function
/// identifier into a unit struct implementing `HttpServiceFactory`, hiding the real handler body
/// as a private nested item. Keeping it around would make it impossible for
/// `generate_register_service` to call the handler directly when it later merges same-path
/// handlers into a single named `Resource`.
fn auto_register_impl(
    registry: &Registry,
    prefix: String,
    mut input_fn: ItemFn,
) -> syn::Result<proc_macro2::TokenStream> {
    let fn_name = input_fn.sig.ident.to_string();

    let mut route_path = None;
    let mut verb = None;

    for attr in &input_fn.attrs {
        if is_http_verb_attr(attr) {
            let segment = attr.path().segments.last().unwrap();
            verb = Some(segment.ident.to_string().to_uppercase());
            if let Ok(route_literal) = attr.parse_args::<LitStr>() {
                route_path = Some(route_literal.value());
            }
        }
    }

    let (route_path, verb) = match (route_path, verb) {
        (Some(path), Some(verb)) => (path, verb),
        _ => {
            return Err(syn::Error::new_spanned(
                &input_fn.sig.ident,
                format!(
                    "Could not extract the route path or verb from attributes on function '{fn_name}'. \
                    Ensure it has a valid Actix route macro like #[get(\"/path\")]."
                ),
            ));
        },
    };

    input_fn.attrs.retain(|attr| !is_http_verb_attr(attr));

    let route_info = RouteInfo {
        prefix: prefix.clone(),
        handler_name: fn_name,
        path: route_path,
        verb,
    };

    registry
        .write()
        .expect("Failed to acquire write lock")
        .entry(prefix)
        .or_default()
        .push(route_info);

    Ok(quote! { #input_fn })
}

#[proc_macro_attribute]
pub fn auto_register(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);

    let prefix = if attr.is_empty() {
        return syn::Error::new(
            Span::call_site(),
            "Expected a prefix (e.g., \"/scope\") as the argument to auto_register",
        )
        .to_compile_error()
        .into();
    } else {
        match syn::parse::<LitStr>(attr) {
            Ok(lit_str) => lit_str.value(),
            Err(err) => return err.to_compile_error().into(),
        }
    };

    match auto_register_impl(&REGISTRATION_MAP, prefix, input_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn parse_register_service_args(args: &syn::ExprArray) -> syn::Result<(String, bool)> {
    let module_key = match args.elems.first() {
        Some(syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(lit_str),
            ..
        })) => lit_str.value(),
        _ => {
            return Err(syn::Error::new_spanned(
                args,
                "Expected the first argument to be a string literal representing the module key.",
            ));
        },
    };

    let mut use_scope = false;
    if let Some(syn::Expr::Assign(syn::ExprAssign { left, right, .. })) = args.elems.iter().nth(1)
        && let syn::Expr::Path(path) = &**left
        && path.path.is_ident("use_scope")
    {
        match &**right {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Bool(lit_bool),
                ..
            }) => use_scope = lit_bool.value(),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "The value of `use_scope` must be a boolean.",
                ));
            },
        }
    }

    Ok((module_key, use_scope))
}

/// Turns an HTTP verb ("GET", "POST", ...) into the matching PascalCase identifier used by
/// `actix_web::guard` (`Get`, `Post`, ...).
fn verb_guard_ident(verb: &str) -> String {
    let mut chars = verb.chars();
    match chars.next() {
        Some(first) => first.to_string() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// Builds the `cfg.service(...)` block for one module.
///
/// Every handler is registered as its own `actix_web::web::resource(...)`, guarded by its HTTP
/// verb (`.guard(actix_web::guard::Get())`, etc.) — mirroring exactly what
/// `actix-web-codegen`'s own `#[get(...)]`/`#[post(...)]` macros generate — but with an explicit
/// `.name(...)` qualified by prefix and path, so handlers sharing a path all report the same
/// name instead of whichever bare function name Actix happened to register first for it.
///
/// Handlers are emitted in the exact order they were declared (`registrations`'s own order,
/// straight from `REGISTRATION_MAP` — no grouping or reordering by path). This matters for two
/// separate reasons, both about Actix trying registered nodes in registration order and
/// stopping at the first one whose path *and* guard both match:
///
/// - `.guard()` (rather than merging same-path handlers into one resource with multiple
///   `.route()`s) keeps a guard mismatch a full non-match for that node, so Actix falls through
///   to try the next one — e.g. a literal `/batch/...` after an unrelated `/{id}/...` that
///   only *coincidentally* matches the same path but not the request's verb.
/// - Preserving declaration order (rather than grouping handlers by path, which would move a
///   path's *first-declared* verb's position to cover every verb registered under it) keeps a
///   more specific static path — like `/me` declared before `/{uuid}` for the same verb, PUT —
///   ahead of the dynamic one that would otherwise swallow it first.
fn build_scope_service_tokens(
    registrations: Vec<RouteInfo>,
    prefix: &str,
    use_scope: bool,
) -> proc_macro2::TokenStream {
    let resource_blocks = registrations.iter().map(|route| {
        let resource_name = format!("{prefix}{}", route.path);
        let path_literal = LitStr::new(&route.path, Span::call_site());
        let name_literal = LitStr::new(&resource_name, Span::call_site());
        let fn_ident = syn::Ident::new(&route.handler_name, Span::call_site());
        let guard_ident = syn::Ident::new(&verb_guard_ident(&route.verb), Span::call_site());

        quote! {
            actix_web::web::resource(#path_literal)
                .name(#name_literal)
                .guard(actix_web::guard::#guard_ident())
                .to(#fn_ident)
        }
    });

    let scope_path = if use_scope { prefix } else { "" };
    let scope_path_literal = LitStr::new(scope_path, Span::call_site());

    quote! {
        cfg.service(
            actix_web::web::scope(#scope_path_literal)
                #(.service(#resource_blocks))*
        );
    }
}

fn generate_register_service_impl(
    registry: &Registry,
    module_key: &str,
    use_scope: bool,
) -> proc_macro2::TokenStream {
    let registrations = registry
        .read()
        .expect("Failed to acquire read lock")
        .get(module_key)
        .cloned()
        .unwrap_or_default();

    let scope_block = build_scope_service_tokens(registrations, module_key, use_scope);

    quote! {
        pub fn register_service(cfg: &mut actix_web::web::ServiceConfig) {
            #scope_block
        }
    }
}

#[proc_macro]
pub fn generate_register_service(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as syn::ExprArray);

    let (module_key, use_scope) = match parse_register_service_args(&args) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };

    generate_register_service_impl(&REGISTRATION_MAP, &module_key, use_scope).into()
}

fn generate_list_routes_impl(registry: &Registry) -> proc_macro2::TokenStream {
    let map = registry.read().expect("Failed to acquire read lock");

    let mut rows = Vec::new();
    for (scope, routes) in map.iter() {
        for route in routes {
            let scope_literal = LitStr::new(scope, Span::call_site());
            let path_literal = LitStr::new(&route.path, Span::call_site());
            let handler_literal = LitStr::new(&route.handler_name, Span::call_site());
            let verb_literal = LitStr::new(&route.verb, Span::call_site());

            rows.push(quote! {
                Route {
                    scope: #scope_literal.to_string(),
                    path: #path_literal.to_string(),
                    handler: #handler_literal.to_string(),
                    verb: #verb_literal.to_string(),
                }
            });
        }
    }

    let wrap_cell_tokens = wrap_cell_fn_tokens();

    quote! {
        pub fn list_routes() {
            struct Route {
                scope: String,
                path: String,
                handler: String,
                verb: String,
            }

            let routes = vec![
                #(#rows),*
            ];

            const MAX_CELL_WIDTH: usize = 30;

            #wrap_cell_tokens

            let headers = ["Scope", "Path", "Handler", "Verb"];
            let mut widths = [
                headers[0].len(),
                headers[1].len(),
                headers[2].len(),
                headers[3].len(),
            ];
            for route in &routes {
                widths[0] = widths[0].max(route.scope.chars().count().min(MAX_CELL_WIDTH));
                widths[1] = widths[1].max(route.path.chars().count().min(MAX_CELL_WIDTH));
                widths[2] = widths[2].max(route.handler.chars().count().min(MAX_CELL_WIDTH));
                widths[3] = widths[3].max(route.verb.chars().count().min(MAX_CELL_WIDTH));
            }

            fn separator(widths: &[usize; 4]) -> String {
                let mut line = String::from("+");
                for width in widths {
                    line.push_str(&"-".repeat(width + 2));
                    line.push('+');
                }
                line
            }

            fn row_lines(cells: [&str; 4], widths: &[usize; 4]) -> Vec<String> {
                let wrapped: [Vec<String>; 4] = [
                    wrap_cell(cells[0], widths[0]),
                    wrap_cell(cells[1], widths[1]),
                    wrap_cell(cells[2], widths[2]),
                    wrap_cell(cells[3], widths[3]),
                ];
                let sublines = wrapped.iter().map(Vec::len).max().unwrap_or(1);
                (0..sublines)
                    .map(|i| {
                        let mut line = String::from("|");
                        for (cell_lines, width) in wrapped.iter().zip(widths.iter()) {
                            let text = cell_lines.get(i).map(String::as_str).unwrap_or("");
                            line.push_str(&format!(" {:<width$} |", text, width = width));
                        }
                        line
                    })
                    .collect()
            }

            let separator = separator(&widths);

            println!("List of the automatically registered routes:");
            println!("{}", separator);
            for line in row_lines(headers, &widths) {
                println!("{}", line);
            }
            println!("{}", separator);
            for route in &routes {
                for line in row_lines(
                    [
                        route.scope.as_str(),
                        route.path.as_str(),
                        route.handler.as_str(),
                        route.verb.as_str(),
                    ],
                    &widths,
                ) {
                    println!("{}", line);
                }
            }
            println!("{}", separator);
        }
    }
}

/// Word-wraps a single table cell to `max_width`, preferring to break right after the last `/`
/// or `_` within the budget so route paths and handler names split on a readable boundary
/// instead of mid-segment.
///
/// This exact logic is duplicated (not called) inside the `quote!` block above: the generated
/// `list_routes` function runs in the *caller's* crate at their runtime, so it needs its own
/// copy of the algorithm rather than a reference to this one. Keep both in sync.
///
/// Only exists for `wrap_cell_tests` to exercise directly; production code goes through
/// `wrap_cell_fn_tokens` instead, hence `cfg(test)`.
#[cfg(test)]
fn wrap_cell(text: &str, max_width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_width || max_width == 0 {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut start = 0;
    while chars.len() - start > max_width {
        let window = &chars[start..start + max_width];
        let break_at = window
            .iter()
            .rposition(|&c| c == '/' || c == '_')
            .map(|i| i + 1)
            .unwrap_or(max_width);
        lines.push(chars[start..start + break_at].iter().collect());
        start += break_at;
    }
    if start < chars.len() {
        lines.push(chars[start..].iter().collect());
    }
    lines
}

fn wrap_cell_fn_tokens() -> proc_macro2::TokenStream {
    quote! {
        fn wrap_cell(text: &str, max_width: usize) -> Vec<String> {
            let chars: Vec<char> = text.chars().collect();
            if chars.len() <= max_width || max_width == 0 {
                return vec![text.to_string()];
            }

            let mut lines = Vec::new();
            let mut start = 0;
            while chars.len() - start > max_width {
                let window = &chars[start..start + max_width];
                let break_at = window
                    .iter()
                    .rposition(|&c| c == '/' || c == '_')
                    .map(|i| i + 1)
                    .unwrap_or(max_width);
                lines.push(chars[start..start + break_at].iter().collect());
                start += break_at;
            }
            if start < chars.len() {
                lines.push(chars[start..].iter().collect());
            }
            lines
        }
    }
}

#[proc_macro]
pub fn generate_list_routes(_input: TokenStream) -> TokenStream {
    generate_list_routes_impl(&REGISTRATION_MAP).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fn(source: &str) -> ItemFn {
        syn::parse_str::<ItemFn>(source).expect("test input must parse as a function")
    }

    fn fresh_registry() -> Registry {
        RwLock::new(HashMap::new())
    }

    fn stored_routes(registry: &Registry, prefix: &str) -> Vec<RouteInfo> {
        registry.read().unwrap().get(prefix).cloned().unwrap_or_default()
    }

    mod auto_register_impl_tests {
        use super::*;

        #[test]
        fn extracts_verb_and_path_and_registers_route() {
            let registry = fresh_registry();
            let input = parse_fn(
                r#"
                #[get("/search")]
                pub async fn search() -> impl Responder { "ok" }
                "#,
            );

            auto_register_impl(&registry, "/events".to_string(), input).unwrap();

            let routes = stored_routes(&registry, "/events");
            assert_eq!(routes.len(), 1);
            assert_eq!(routes[0].handler_name, "search");
            assert_eq!(routes[0].path, "/search");
            assert_eq!(routes[0].verb, "GET");
            assert_eq!(routes[0].prefix, "/events");
        }

        #[test]
        fn strips_the_actix_verb_attribute_from_the_emitted_function() {
            let registry = fresh_registry();
            let input = parse_fn(
                r#"
                #[get("/search")]
                pub async fn search() -> impl Responder { "ok" }
                "#,
            );

            let output = auto_register_impl(&registry, "/events".to_string(), input).unwrap();
            let output_fn: ItemFn = syn::parse2(output).unwrap();

            assert!(
                output_fn.attrs.iter().all(|attr| !is_http_verb_attr(attr)),
                "the actix route attribute must not survive in the emitted function, \
                otherwise actix-web-codegen would turn it into a unit struct and hide the handler",
            );
        }

        #[test]
        fn preserves_other_attributes_on_the_function() {
            let registry = fresh_registry();
            let input = parse_fn(
                r#"
                #[tracing::instrument]
                #[get("/search")]
                pub async fn search() -> impl Responder { "ok" }
                "#,
            );

            let output = auto_register_impl(&registry, "/events".to_string(), input).unwrap();
            let output_fn: ItemFn = syn::parse2(output).unwrap();

            assert_eq!(output_fn.attrs.len(), 1);
            assert_eq!(
                output_fn.attrs[0].path().segments.last().unwrap().ident.to_string(),
                "instrument"
            );
        }

        #[test]
        fn preserves_function_signature_and_body() {
            let registry = fresh_registry();
            let input = parse_fn(
                r#"
                #[post("/create")]
                pub async fn create(body: web::Json<Dto>) -> impl Responder { do_thing(body) }
                "#,
            );

            let output = auto_register_impl(&registry, "/events".to_string(), input).unwrap();
            let output_fn: ItemFn = syn::parse2(output).unwrap();

            assert_eq!(output_fn.sig.ident.to_string(), "create");
            assert_eq!(output_fn.sig.inputs.len(), 1);
        }

        #[test]
        fn each_http_verb_is_uppercased_correctly() {
            for (attr, expected_verb) in [
                ("get", "GET"),
                ("post", "POST"),
                ("put", "PUT"),
                ("delete", "DELETE"),
                ("patch", "PATCH"),
            ] {
                let registry = fresh_registry();
                let input = parse_fn(&format!(
                    r#"
                    #[{attr}("/x")]
                    pub async fn handler() -> impl Responder {{ "ok" }}
                    "#
                ));

                auto_register_impl(&registry, "/p".to_string(), input).unwrap();

                let routes = stored_routes(&registry, "/p");
                assert_eq!(routes[0].verb, expected_verb);
            }
        }

        #[test]
        fn errors_when_no_actix_route_attribute_is_present() {
            let registry = fresh_registry();
            let input = parse_fn(
                r#"
                pub async fn search() -> impl Responder { "ok" }
                "#,
            );

            let err = auto_register_impl(&registry, "/events".to_string(), input).unwrap_err();
            assert!(err.to_string().contains("Could not extract the route path or verb"));
        }

        #[test]
        fn errors_when_route_literal_is_not_a_string() {
            let registry = fresh_registry();
            let input = parse_fn(
                r#"
                #[get(some_ident)]
                pub async fn search() -> impl Responder { "ok" }
                "#,
            );

            let err = auto_register_impl(&registry, "/events".to_string(), input).unwrap_err();
            assert!(err.to_string().contains("Could not extract the route path or verb"));
        }

        #[test]
        fn accumulates_multiple_handlers_under_the_same_prefix() {
            let registry = fresh_registry();

            auto_register_impl(
                &registry,
                "/events".to_string(),
                parse_fn(r#"#[get("")] pub async fn find() -> impl Responder { "ok" }"#),
            )
            .unwrap();
            auto_register_impl(
                &registry,
                "/events".to_string(),
                parse_fn(r#"#[post("")] pub async fn create() -> impl Responder { "ok" }"#),
            )
            .unwrap();

            let routes = stored_routes(&registry, "/events");
            assert_eq!(routes.len(), 2);
            assert_eq!(routes[0].handler_name, "find");
            assert_eq!(routes[1].handler_name, "create");
        }

        #[test]
        fn same_handler_name_under_different_prefixes_does_not_collide() {
            let registry = fresh_registry();

            auto_register_impl(
                &registry,
                "/equipments".to_string(),
                parse_fn(r#"#[post("")] pub async fn create() -> impl Responder { "ok" }"#),
            )
            .unwrap();
            auto_register_impl(
                &registry,
                "/bookings".to_string(),
                parse_fn(r#"#[post("")] pub async fn create() -> impl Responder { "ok" }"#),
            )
            .unwrap();

            assert_eq!(stored_routes(&registry, "/equipments").len(), 1);
            assert_eq!(stored_routes(&registry, "/bookings").len(), 1);
        }
    }

    mod parse_register_service_args_tests {
        use super::*;

        fn parse_array(source: &str) -> syn::ExprArray {
            syn::parse_str::<syn::ExprArray>(source).expect("test input must parse as an array")
        }

        #[test]
        fn parses_module_key_only() {
            let args = parse_array(r#"["/events"]"#);
            let (module_key, use_scope) = parse_register_service_args(&args).unwrap();
            assert_eq!(module_key, "/events");
            assert!(!use_scope);
        }

        #[test]
        fn parses_use_scope_true() {
            let args = parse_array(r#"["/events", use_scope = true]"#);
            let (module_key, use_scope) = parse_register_service_args(&args).unwrap();
            assert_eq!(module_key, "/events");
            assert!(use_scope);
        }

        #[test]
        fn parses_use_scope_false_explicitly() {
            let args = parse_array(r#"["/events", use_scope = false]"#);
            let (_, use_scope) = parse_register_service_args(&args).unwrap();
            assert!(!use_scope);
        }

        #[test]
        fn errors_when_first_argument_is_not_a_string_literal() {
            let args = parse_array(r#"[42]"#);
            let err = parse_register_service_args(&args).unwrap_err();
            assert!(err.to_string().contains("string literal"));
        }

        #[test]
        fn errors_when_use_scope_is_not_a_boolean() {
            let args = parse_array(r#"["/events", use_scope = "yes"]"#);
            let err = parse_register_service_args(&args).unwrap_err();
            assert!(err.to_string().contains("must be a boolean"));
        }

        #[test]
        fn ignores_unrelated_assignment_keys() {
            let args = parse_array(r#"["/events", other_flag = true]"#);
            let (module_key, use_scope) = parse_register_service_args(&args).unwrap();
            assert_eq!(module_key, "/events");
            assert!(!use_scope);
        }
    }

    mod build_scope_service_tokens_tests {
        use super::*;

        fn route(prefix: &str, handler: &str, path: &str, verb: &str) -> RouteInfo {
            RouteInfo {
                prefix: prefix.to_string(),
                handler_name: handler.to_string(),
                path: path.to_string(),
                verb: verb.to_string(),
            }
        }

        #[test]
        fn distinct_paths_get_one_guarded_resource_each() {
            let routes = vec![
                route("/equipments", "find", "", "GET"),
                route("/equipments", "find_one", "/{uuid}", "GET"),
            ];

            let tokens = build_scope_service_tokens(routes, "/equipments", true);

            let expected = quote! {
                cfg.service(
                    actix_web::web::scope("/equipments")
                        .service(
                            actix_web::web::resource("")
                                .name("/equipments")
                                .guard(actix_web::guard::Get())
                                .to(find)
                        )
                        .service(
                            actix_web::web::resource("/{uuid}")
                                .name("/equipments/{uuid}")
                                .guard(actix_web::guard::Get())
                                .to(find_one)
                        )
                );
            };

            assert_eq!(tokens.to_string(), expected.to_string());
        }

        #[test]
        fn handlers_sharing_a_path_become_separate_guarded_resources_with_the_same_name() {
            // Regression test for two distinct bugs:
            // 1. actix's `match_name()` used to report whichever bare function name was
            //    registered first for a shared path, for every HTTP method hitting it — fixed
            //    by giving every handler on the same path the same qualified `.name(...)`.
            // 2. Merging them into `.route()`s on a single resource fixed (1) but broke
            //    fallthrough to sibling resources whose path pattern only *coincidentally*
            //    matches (e.g. a dynamic `/{id}/...` swallowing a literal `/batch/...`) — a
            //    `.route()`-based resource commits to the node once its path matches and
            //    returns 405 instead of trying the next sibling. `.guard()` keeps each handler
            //    as its own resource, so a guard mismatch is a full non-match, preserving the
            //    fallthrough exactly like actix-web-codegen's own `#[get]`/`#[post]` macros do.
            let routes = vec![
                route("/equipments", "create", "", "POST"),
                route("/equipments", "find", "", "GET"),
            ];

            let tokens = build_scope_service_tokens(routes, "/equipments", true);

            let expected = quote! {
                cfg.service(
                    actix_web::web::scope("/equipments")
                        .service(
                            actix_web::web::resource("")
                                .name("/equipments")
                                .guard(actix_web::guard::Post())
                                .to(create)
                        )
                        .service(
                            actix_web::web::resource("")
                                .name("/equipments")
                                .guard(actix_web::guard::Get())
                                .to(find)
                        )
                );
            };

            assert_eq!(tokens.to_string(), expected.to_string());
        }

        #[test]
        fn resource_names_are_qualified_by_prefix_and_path() {
            let routes = vec![route("/deposits", "create", "", "POST")];

            let tokens = build_scope_service_tokens(routes, "/deposits", true);
            let rendered = tokens.to_string();

            assert!(rendered.contains("\"/deposits\""));
        }

        #[test]
        fn every_supported_verb_maps_to_its_pascal_case_guard() {
            for (verb, guard) in [
                ("GET", "Get"),
                ("POST", "Post"),
                ("PUT", "Put"),
                ("DELETE", "Delete"),
                ("PATCH", "Patch"),
            ] {
                let routes = vec![route("/x", "handler", "", verb)];
                let tokens = build_scope_service_tokens(routes, "/x", true);
                assert!(
                    tokens.to_string().contains(&format!("guard :: {guard} ()")),
                    "expected guard {guard} for verb {verb}, got: {tokens}"
                );
            }
        }

        #[test]
        fn use_scope_false_mounts_under_an_empty_scope() {
            let routes = vec![route("/equipments", "find", "", "GET")];

            let tokens = build_scope_service_tokens(routes, "/equipments", false);
            let expected = quote! {
                cfg.service(
                    actix_web::web::scope("")
                        .service(
                            actix_web::web::resource("")
                                .name("/equipments")
                                .guard(actix_web::guard::Get())
                                .to(find)
                        )
                );
            };

            assert_eq!(tokens.to_string(), expected.to_string());
        }

        #[test]
        fn declaration_order_is_preserved_even_when_an_earlier_dynamic_path_reappears_later() {
            // Regression test: a naive "group handlers by path" strategy would move every
            // verb registered under a path to that path's *first* declaration point. Here,
            // `find_one` (GET "/{uuid}") is declared before `update_me` (PUT "/me"), so grouping
            // by path would register the PUT-guarded "/{uuid}" resource (from `update`, declared
            // after `update_me`) ahead of "/me" — letting "/{uuid}" swallow "me" as its dynamic
            // segment before Actix ever tries the more specific static "/me" route, since both
            // are guarded on the same verb, PUT, so there's no guard mismatch to fall through on.
            // Declaration order must be preserved exactly as-is, with no path-based grouping.
            let routes = vec![
                route("/users", "find_one", "/{uuid}", "GET"),
                route("/users", "update_me", "/me", "PUT"),
                route("/users", "update", "/{uuid}", "PUT"),
            ];

            let tokens = build_scope_service_tokens(routes, "/users", true);

            let expected = quote! {
                cfg.service(
                    actix_web::web::scope("/users")
                        .service(
                            actix_web::web::resource("/{uuid}")
                                .name("/users/{uuid}")
                                .guard(actix_web::guard::Get())
                                .to(find_one)
                        )
                        .service(
                            actix_web::web::resource("/me")
                                .name("/users/me")
                                .guard(actix_web::guard::Put())
                                .to(update_me)
                        )
                        .service(
                            actix_web::web::resource("/{uuid}")
                                .name("/users/{uuid}")
                                .guard(actix_web::guard::Put())
                                .to(update)
                        )
                );
            };

            assert_eq!(tokens.to_string(), expected.to_string());
        }

        #[test]
        fn empty_registrations_produce_an_empty_scope() {
            let tokens = build_scope_service_tokens(vec![], "/equipments", true);
            let expected = quote! {
                cfg.service(
                    actix_web::web::scope("/equipments")
                );
            };

            assert_eq!(tokens.to_string(), expected.to_string());
        }
    }

    mod generate_register_service_impl_tests {
        use super::*;

        #[test]
        fn generates_a_register_service_function_wrapping_the_scope_block() {
            let registry = fresh_registry();
            auto_register_impl(
                &registry,
                "/events".to_string(),
                parse_fn(r#"#[get("/search")] pub async fn search() -> impl Responder { "ok" }"#),
            )
            .unwrap();

            let tokens = generate_register_service_impl(&registry, "/events", true);
            let parsed: ItemFn = syn::parse2(tokens).expect("must parse as a function");

            assert_eq!(parsed.sig.ident.to_string(), "register_service");
        }

        #[test]
        fn unknown_module_key_generates_an_empty_but_valid_scope() {
            let registry = fresh_registry();
            let tokens = generate_register_service_impl(&registry, "/unknown", true);
            let parsed: syn::Result<ItemFn> = syn::parse2(tokens);
            assert!(parsed.is_ok());
        }
    }

    mod generate_list_routes_impl_tests {
        use super::*;

        #[test]
        fn generates_a_list_routes_function() {
            let registry = fresh_registry();
            auto_register_impl(
                &registry,
                "/events".to_string(),
                parse_fn(r#"#[get("/search")] pub async fn search() -> impl Responder { "ok" }"#),
            )
            .unwrap();

            let tokens = generate_list_routes_impl(&registry);
            let parsed: ItemFn = syn::parse2(tokens).expect("must parse as a function");

            assert_eq!(parsed.sig.ident.to_string(), "list_routes");
        }

        #[test]
        fn output_compiles_with_no_registrations() {
            let registry = fresh_registry();
            let tokens = generate_list_routes_impl(&registry);
            let parsed: syn::Result<ItemFn> = syn::parse2(tokens);
            assert!(parsed.is_ok());
        }

        #[test]
        fn embeds_a_wrap_cell_helper_matching_the_standalone_copy() {
            let registry = fresh_registry();
            let generated = generate_list_routes_impl(&registry);
            let standalone = wrap_cell_fn_tokens();

            assert!(
                generated.to_string().contains(&standalone.to_string()),
                "the copy of wrap_cell embedded in the generated code must stay in sync with \
                the standalone one covered by wrap_cell_tests",
            );
        }
    }

    mod wrap_cell_tests {
        use super::*;

        #[test]
        fn text_within_max_width_is_not_wrapped() {
            assert_eq!(wrap_cell("short", 30), vec!["short".to_string()]);
        }

        #[test]
        fn text_exactly_at_max_width_is_not_wrapped() {
            assert_eq!(wrap_cell("exact", 5), vec!["exact".to_string()]);
        }

        #[test]
        fn breaks_after_the_last_slash_within_budget() {
            let wrapped = wrap_cell("/user_uuid/organizations/organization_uuid/suspend", 20);
            assert_eq!(
                wrapped,
                vec![
                    "/user_uuid/".to_string(),
                    "organizations/".to_string(),
                    "organization_uuid/".to_string(),
                    "suspend".to_string(),
                ]
            );
            assert!(wrapped.iter().all(|line| line.chars().count() <= 20));
        }

        #[test]
        fn breaks_after_the_last_underscore_when_no_slash_is_available() {
            let wrapped = wrap_cell("get_roles_organizations_of_user", 10);
            assert!(wrapped.iter().all(|line| line.chars().count() <= 10));
            assert_eq!(wrapped.concat(), "get_roles_organizations_of_user");
        }

        #[test]
        fn hard_cuts_when_no_separator_is_available_within_budget() {
            let wrapped = wrap_cell("abcdefghijklmnop", 4);
            assert_eq!(
                wrapped,
                vec!["abcd".to_string(), "efgh".to_string(), "ijkl".to_string(), "mnop".to_string()]
            );
        }

        #[test]
        fn max_width_zero_falls_back_to_a_single_line() {
            assert_eq!(wrap_cell("anything", 0), vec!["anything".to_string()]);
        }

        #[test]
        fn empty_text_yields_a_single_empty_line() {
            assert_eq!(wrap_cell("", 10), vec!["".to_string()]);
        }

        #[test]
        fn is_safe_on_multi_byte_utf8_content() {
            let wrapped = wrap_cell("café/du_matin/à_paris", 6);
            assert!(wrapped.iter().all(|line| line.chars().count() <= 6));
            assert_eq!(wrapped.concat(), "café/du_matin/à_paris");
        }

        #[test]
        fn reassembling_wrapped_lines_reproduces_the_original_text() {
            let original = "/{user_uuid}/organizations/{organization_uuid}/suspend";
            let wrapped = wrap_cell(original, 15);
            assert_eq!(wrapped.concat(), original);
        }
    }
}
