extern crate proc_macro;

use once_cell::sync::Lazy;
use proc_macro::TokenStream;
use quote::quote;
use std::sync::RwLock;
use syn::{parse_macro_input, ItemFn};

// Use a global RwLock map for storing registrations per unique module key
static REGISTRATION_MAP: Lazy<RwLock<std::collections::HashMap<String, Vec<(String, String)>>>> =
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
        panic!("Expected a prefix (e.g., \"/events\") as the argument to auto_register");
    };

    // Use the prefix as the module key
    let module_key = prefix.clone();

    // Safely store the prefix and function name in a module-specific collection
    let mut map = REGISTRATION_MAP
        .write()
        .expect("Failed to acquire write lock");
    map.entry(module_key.clone())
        .or_insert_with(Vec::new)
        .push((prefix.clone(), fn_name.clone()));

    // Generate the original function definition
    let expanded = quote! {
        #input_fn
    };

    TokenStream::from(expanded)
}

#[proc_macro]
pub fn generate_register_service(input: TokenStream) -> TokenStream {
    // Parse the prefix (module key) from the macro input
    let module_key = parse_macro_input!(input as syn::LitStr).value();

    // Safely read handler registrations for the specified module key
    let map = REGISTRATION_MAP
        .read()
        .expect("Failed to acquire read lock");
    let registrations = map.get(&module_key).cloned().unwrap_or_default();

    // Group functions by their prefixes
    let mut grouped_by_prefix: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (prefix, fn_name) in registrations {
        grouped_by_prefix
            .entry(prefix.clone())
            .or_insert_with(Vec::new)
            .push(fn_name);
    }

    // Generate the registration function code
    let mut registration_functions = Vec::new();
    for (_, functions) in grouped_by_prefix {
        let fn_calls = functions.iter().map(|fn_name| {
            let fn_ident = syn::Ident::new(fn_name, proc_macro2::Span::call_site());
            quote! {
                .service(#fn_ident)
            }
        });

        let scope_block = quote! {
            cfg.service(
                actix_web::web::scope("")
                    #(#fn_calls)*
            );
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

    // Generate code to log all routes
    let mut route_logs = Vec::new();

    for (prefix, routes) in map.iter() {
        let prefix_literal = syn::LitStr::new(prefix, proc_macro2::Span::call_site());
        let route_entries = routes.iter().map(|(_, fn_name)| {
            let fn_literal = syn::LitStr::new(fn_name, proc_macro2::Span::call_site());
            quote! {
                println!("  Route: {}", #fn_literal);
            }
        });

        route_logs.push(quote! {
            println!("Scope: {}", #prefix_literal);
            #(#route_entries)*
        });
    }

    // Generate the `list_routes` function
    let expanded = quote! {
        pub fn list_routes() {
            println!("List of automatically generated routes");
            #(#route_logs)*
        }
    };

    TokenStream::from(expanded)
}
