//! Handler resolvers: the chain between a `HandlerRef` and a runnable handler.

pub mod builtin_resolver;
pub mod composite_resolver;
pub mod http_resolver;
pub mod script_resolver;

pub use builtin_resolver::BuiltinHandlerResolver;
pub use composite_resolver::CompositeHandlerResolver;
pub use http_resolver::HttpHandlerResolver;
pub use script_resolver::ScriptHandlerResolver;
