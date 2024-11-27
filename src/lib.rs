extern crate proc_macro;
extern crate tabled;
use once_cell::sync::Lazy;
use proc_macro::TokenStream;
use quote::quote;
use std::sync::RwLock;
use syn::{parse_macro_input, ItemFn, LitStr};

#[derive(Debug, Clone)]
struct RouteInfo {
    prefix: String,       // The scope or module key (e.g., "/events")
    handler_name: String, // The name of the handler function
    path: String,         // The route path (e.g., "/search")
    verb: String,         // The HTTP method (e.g., "GET")
}

// Use a global RwLock map for storing registrations per unique module key
static REGISTRATION_MAP: Lazy<RwLock<std::collections::HashMap<String, Vec<RouteInfo>>>> =
    Lazy::new(|| RwLock::new(std::collections::HashMap::new()));

#[proc_macro_attribute]
pub fn auto_register(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse the input function
    let input_fn = parse_macro_input!(item as ItemFn);
    let fn_name = input_fn.sig.ident.to_string();

    // Parse the prefix as a string literal
    let prefix = if !attr.is_empty() {
        let parsed_attr = parse_macro_input!(attr as syn::LitStr);
        parsed_attr.value()
    } else {
        panic!("Expected a prefix (e.g., \"/scope\") as the argument to auto_register");
    };

    // Extract the route path and HTTP verb from the function attributes
    let mut route_path = None;
    let mut verb = None;

    for attr in &input_fn.attrs {
        if let Some(segment) = attr.path().segments.last() {
            if ["get", "post", "put", "delete", "patch"]
                .contains(&segment.ident.to_string().as_str())
            {
                verb = Some(segment.ident.to_string().to_uppercase());
                if let Ok(route_literal) = attr.parse_args::<LitStr>() {
                    route_path = Some(route_literal.value());
                }
            }
        }
    }

    // Validate the extracted route path and HTTP verb
    if route_path.is_none() || verb.is_none() {
        panic!(
            "Could not extract the route path or verb from attributes on function '{}'. Ensure it has a valid Actix route macro like \
            #[get(\"/path\")].",
            fn_name
        );
    }

    // Use empty route path if valid (e.g., `""`)
    let route_info = RouteInfo {
        prefix: prefix.clone(),
        handler_name: fn_name.clone(),
        path: route_path.unwrap_or_else(|| "".to_string()),
        verb: verb.unwrap(),
    };

    // Safely store the route information
    let mut map = REGISTRATION_MAP
        .write()
        .expect("Failed to acquire write lock");
    map.entry(prefix.clone()).or_default().push(route_info);

    // Generate the original function definition
    let expanded = quote! {
        #input_fn
    };

    TokenStream::from(expanded)
}

#[proc_macro]
pub fn generate_register_service(input: TokenStream) -> TokenStream {
    // Parse the macro arguments (prefix and optional use_scope flag)
    let args = parse_macro_input!(input as syn::ExprArray);
    let module_key: Option<String>;
    let mut use_scope = false; // Default to not using the prefix as the scope

    // Parse the arguments
    if let Some(syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(lit_str),
        ..
    })) = args.elems.iter().next()
    {
        module_key = Some(lit_str.value());
    } else {
        panic!("Expected the first argument to be a string literal representing the module key.");
    }

    if let Some(syn::Expr::Assign(syn::ExprAssign { left, right, .. })) = args.elems.iter().nth(1) {
        if let syn::Expr::Path(path) = &**left {
            if path.path.is_ident("use_scope") {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Bool(lit_bool),
                    ..
                }) = &**right
                {
                    use_scope = lit_bool.value();
                } else {
                    panic!("The value of `use_scope` must be a boolean.");
                }
            }
        }
    }

    if module_key.is_none() {
        panic!("Expected a module key as the first argument.");
    }

    // Safely read handler registrations for the specified module key
    let map = REGISTRATION_MAP
        .read()
        .expect("Failed to acquire read lock");
    let registrations = map.get(&module_key.unwrap()).cloned().unwrap_or_default();

    // Group functions by their prefixes
    let mut grouped_by_prefix: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for RouteInfo {
        prefix,
        handler_name,
        ..
    } in registrations
    {
        grouped_by_prefix
            .entry(prefix.clone())
            .or_default()
            .push(handler_name);
    }

    // Generate the registration function code
    let mut registration_functions = Vec::new();
    for (prefix, functions) in grouped_by_prefix {
        let fn_calls = functions.iter().map(|fn_name| {
            let fn_ident = syn::Ident::new(fn_name, proc_macro2::Span::call_site());
            quote! {
                .service(#fn_ident)
            }
        });

        let scope_block = if use_scope {
            quote! {
                cfg.service(
                    actix_web::web::scope(#prefix)
                        #(#fn_calls)*
                );
            }
        } else {
            quote! {
                cfg.service(
                    actix_web::web::scope("")
                        #(#fn_calls)*
                );
            }
        };

        registration_functions.push(scope_block);
    }

    let expanded = quote! {
        pub fn register_service(cfg: &mut actix_web::web::ServiceConfig) {
            #(#registration_functions)*
        }
    };

    TokenStream::from(expanded)
}

#[proc_macro]
pub fn generate_list_routes(_input: TokenStream) -> TokenStream {
    // Safely read all handler registrations from the REGISTRATION_MAP
    let map = REGISTRATION_MAP
        .read()
        .expect("Failed to acquire read lock");

    // Collect all routes into a vector for table display
    let mut rows = Vec::new();
    for (scope, routes) in map.iter() {
        for route in routes {
            let scope_literal = syn::LitStr::new(scope, proc_macro2::Span::call_site());
            let path_literal = syn::LitStr::new(&route.path, proc_macro2::Span::call_site());
            let handler_literal =
                syn::LitStr::new(&route.handler_name, proc_macro2::Span::call_site());
            let verb_literal = syn::LitStr::new(&route.verb, proc_macro2::Span::call_site());

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

    // Generate code for the `list_routes` function
    let expanded = quote! {
        pub fn list_routes() {
            use tabled::{Table, Tabled};

            #[derive(Tabled)]
            struct Route {
                #[tabled(rename = "Scope")]
                scope: String,
                #[tabled(rename = "Path")]
                path: String,
                #[tabled(rename = "Handler")]
                handler: String,
                #[tabled(rename = "Verb")]
                verb: String,
            }

            let routes = vec![
                #(#rows),*
            ];

            let table = Table::new(routes)
                .with(tabled::settings::Style::modern())
                .to_string();

            println!("List of the automatically registered routes:");
            println!("{}", table);
        }
    };

    TokenStream::from(expanded)
}
