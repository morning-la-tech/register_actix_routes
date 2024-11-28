# `register_routes` - A Proc Macro for Dynamic Route Registration in Actix Web

`register_routes` is a Rust proc macro library designed to simplify the registration of routes in an Actix Web application. It enables automatic grouping and configuration of routes, reducing boilerplate code while maintaining scalability and clarity in route management.

---

## Features

1. **Automatic Route Registration**:
    - Use `#[auto_register("/prefix")]` to annotate your handler functions and group them by scope.

2. **Dynamic Service Configuration**:
    - Generate `register_service` functions that automatically configure routes under their respective scopes.

3. **Route Listing**:
    - Dynamically list all registered routes at runtime with `list_routes`.

4. **Support for All HTTP Verbs**:
    - Works seamlessly with `#[get]`, `#[post]`, `#[put]`, `#[delete]`, and `#[patch]`.

5. **Error Handling and Debugging**:
    - Provides detailed error messages if routes are misconfigured or missing attributes.

6. **Customizable Tabled Output**:
    - Display all routes in a clean, tabular format using the `tabled` crate.

---

## Installation

Add the `register_routes` crate to your project:

```toml
[dependencies]
register_routes = "0.1.1"
tabled = "0.17.0" # Required for route listing
```

---

## Usage

### 1. Annotate Handlers with `#[auto_register]`

Add `#[auto_register("/scope")]` to your handler functions to group them by a shared prefix.

```rust
use actix_web::{get, web, Responder};
use register_routes::auto_register;

#[auto_register("/events")]
#[get("/search")]
pub async fn search() -> impl Responder {
    "Search handler"
}

#[auto_register("/events")]
#[post("/create")]
pub async fn create() -> impl Responder {
    "Create handler"
}
```

---

### 2. Generate `register_service`

Use the `generate_register_service` macro to create a function that registers all handlers for a specific scope.
You'll need it at the end of your handlers file.

When integrating the configuration with an existing scope

```rust
use register_routes::generate_register_service;

generate_register_service!(["/events"]);
```

This will generate a `register_service` function like:

```rust
pub fn register_service(cfg: &mut actix_web::web::ServiceConfig) {
    cfg.service(
        actix_web::web::scope("")
            .service(search)
            .service(create)
    );
}
```

If you need the scope created with the path:

```rust
use register_routes::generate_register_service;

generate_register_service!(["/events", use_scope = true ]);
```

This will generate a `register_service` function like:

```rust
pub fn register_service(cfg: &mut actix_web::web::ServiceConfig) {
    cfg.service(
        actix_web::web::scope("/events")
            .service(search)
            .service(create)
    );
}
```

---

### 3. Configure Actix Web Application

Use the generated `register_service` functions to configure your Actix Web app:

Here an example if you used use_scope = true to generate a scoped list of services

```rust
use actix_web::{App, HttpServer};
use crate::handlers::event_handler::register_service as register_event_handlers;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| {
        App::new()
            .configure(register_event_handlers) // Register event handlers
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}
```

---

### 4. List All Registered Routes

Use the `generate_list_routes` macro to display all automatically registered routes in a tabular format.

```rust
use register_routes::generate_list_routes;

generate_list_routes!();
```

Call the `list_routes` function at application startup:

```rust
fn main() {
    list_routes();
}
```

This will print:

```
List of the automatically registered routes:
+--------------------+----------------+----------------+-------+
| Scope              | Path           | Handler        | Verb  |
+--------------------+----------------+----------------+-------+
| /events            | /search        | search         | GET   |
| /events            | /create        | create         | POST  |
+--------------------+----------------+----------------+-------+
```

---

## Error Handling

The macros provide clear error messages for common mistakes:
- **Missing HTTP Verb or Path**: Ensure each handler has a valid Actix Web route macro (e.g., `#[get("/path")]`).
- **Invalid Scope**: The `auto_register` attribute requires a valid scope prefix (e.g., `#[auto_register("/events")]`).

---

## Advanced Usage

### Custom Middleware
You can wrap entire scopes with middleware while still using `register_service`:

```rust
use actix_web::middleware::Logger;

HttpServer::new(|| {
    App::new()
        .wrap(Logger::default())
        .configure(register_event_handlers)
})
```

### Combine Scopes
You can use multiple `register_service` functions to organize routes by feature:

```rust
use crate::handlers::{
    event_handler::register_service as register_event_handlers,
    booking_handler::register_service as register_booking_handlers,
};

HttpServer::new(|| {
    App::new()
        .configure(register_event_handlers)
        .configure(register_booking_handlers)
})
```

---

## Limitations

1. **Actix-Specific**:
    - The macros rely on Actix Web’s routing macros and are not compatible with other frameworks.

2. **Requires `tabled`**:
    - The route listing feature depends on the `tabled` crate for pretty output.

---

## Contributing

Contributions, issues, and feature requests are welcome! To contribute:

1. Fork the repository.
2. Create a new branch for your feature/bug fix.
3. Submit a pull request with detailed information.

---

## License

This project is licensed under the MIT License.

---

Enjoy clean and scalable route management in your Actix Web projects! 🚀
"""