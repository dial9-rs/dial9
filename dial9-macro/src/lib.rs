use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{ExprClosure, ItemFn, Path, Token, parse_macro_input};

enum ConfigSource {
    Path(Path),
    Closure(ExprClosure),
}

struct MainArgs {
    config: ConfigSource,
}

const MISSING_CONFIG_HELP: &str = "missing required `config` argument, e.g.\n  \
                           #[dial9_tokio_telemetry::main(config = my_config_fn)]\n\
                           or with an inline closure:\n  \
                           #[dial9_tokio_telemetry::main(config = || Dial9Config::builder().on_disk_buffer(...).max_total_size(...).build().unwrap())]";

const CONFIG_MUST_BE_ZERO_ARG_HELP: &str = "`config` must be a zero-argument function path or a zero-argument closure, e.g.\n  \
                           #[dial9_tokio_telemetry::main(config = my_config_fn)]\n\
                           or with an inline closure:\n  \
                           #[dial9_tokio_telemetry::main(config = || Dial9Config::builder().on_disk_buffer(...).max_total_size(...).build().unwrap())]";
impl Parse for MainArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error(MISSING_CONFIG_HELP));
        }
        let ident: syn::Ident = input.parse()?;
        if ident != "config" {
            return Err(syn::Error::new(ident.span(), MISSING_CONFIG_HELP));
        }
        input.parse::<Token![=]>()?;

        let config = if input.peek(Token![|]) || input.peek(Token![move]) {
            let closure: ExprClosure = input.parse()?;
            if !closure.inputs.is_empty() {
                return Err(syn::Error::new_spanned(
                    &closure.inputs,
                    CONFIG_MUST_BE_ZERO_ARG_HELP,
                ));
            }
            ConfigSource::Closure(closure)
        } else {
            ConfigSource::Path(input.parse()?)
        };

        if !input.is_empty() {
            return Err(input.error(CONFIG_MUST_BE_ZERO_ARG_HELP));
        }
        Ok(MainArgs { config })
    }
}

fn expand_main(args: MainArgs, input: ItemFn) -> Result<TokenStream2, syn::Error> {
    if input.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            input.sig.fn_token,
            "the `async` keyword is missing from the function declaration",
        ));
    }

    if !input.sig.inputs.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.sig.inputs,
            "#[dial9_tokio_telemetry::main] does not support function arguments",
        ));
    }

    if !input.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.sig.generics,
            "#[dial9_tokio_telemetry::main] does not support generics",
        ));
    }

    if input.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &input.sig.generics.where_clause,
            "#[dial9_tokio_telemetry::main] does not support where clauses",
        ));
    }

    let config_call = match &args.config {
        ConfigSource::Path(p) => quote! { #p() },
        ConfigSource::Closure(c) => quote! { (#c)() },
    };
    let attrs = &input.attrs;
    let vis = &input.vis;
    let name = &input.sig.ident;
    let ret = &input.sig.output;
    let body_stmts = &input.block.stmts;

    Ok(quote! {
        #(#attrs)*
        #vis fn #name() #ret {
            let __dial9_rt = ::dial9_tokio_telemetry::TracedRuntime::new(#config_call);
            let __dial9_out = __dial9_rt.block_on(async move { #(#body_stmts)* });
            __dial9_rt.graceful_shutdown();
            __dial9_out
        }
    })
}

/// Instrument an async main function with dial9 telemetry.
///
/// This macro is a **replacement** for `#[tokio::main]`, not a complement —
/// do not use both attributes on the same function. It builds the Tokio
/// runtime internally and wraps the function body in a spawned task so that
/// poll events are recorded by dial9. Without this, code running directly in
/// `runtime.block_on(...)` is invisible to the telemetry hooks.
///
/// To spawn sub-tasks with wake-event tracking from anywhere inside the
/// body, call `Dial9TokioHandle::current()` — the handle is installed on
/// every runtime-owned thread by `on_thread_start`.
///
/// # Arguments
///
/// * `config` — a zero-argument function path or a zero-argument closure
///   returning any value convertible into a `TracedRuntime`. In
///   practice that means one of:
///     - `Dial9Config` from `Dial9Config::builder().on_disk_buffer(..)..build()`
///       or `..in_memory_buffer()..build()` (strict): a writer-I/O failure surfaces
///       from `.build()` as a `Dial9ConfigBuilderError`; runtime construction
///       under the macro panics on tokio-builder or telemetry-core I/O.
///     - `Dial9Config` from `..build_or_disabled()` (lenient): the same
///       `Dial9Config` type, but writer-I/O failures are logged at `error!`
///       and downgraded to a disabled config that still preserves your
///       `with_tokio` configurators.
///
///   Use `.enabled(false)` to run without telemetry while keeping your
///   `with_tokio` configurators.
///
/// # Graceful shutdown
///
/// After the async body returns, the macro drops the runtime (so Tokio worker
/// threads exit and flush their thread-local buffers) and then performs a
/// graceful shutdown of the telemetry guard, draining the background worker so
/// the final segment is symbolized, compressed, and uploaded before the process
/// exits. The deadline defaults to 1 second and is configurable on the config
/// builder:
///
/// ```rust,ignore
/// Dial9Config::builder()
///     .on_disk_buffer("/tmp/trace.bin")
///     .max_total_size(16 * 1024 * 1024)
///     .graceful_shutdown(std::time::Duration::from_secs(5)) // custom deadline
///     // .disable_graceful_shutdown()                       // or opt out entirely
///     .build()
/// ```
///
/// The low-level `TracedRuntime` API is unaffected — there you call
/// `TelemetryGuard::graceful_shutdown` yourself.
///
/// The implicit drain only runs when the body returns normally. If the body
/// panics, the panic propagates and the guard's `Drop` still flushes and seals
/// the final segment, but the background worker is not drained — so a panicking
/// program may not symbolize or upload its last segment.
///
/// # Examples
///
/// Using a named function:
///
/// ```rust,ignore
/// use dial9_tokio_telemetry::{main, Dial9Config, telemetry::Dial9TokioHandle};
///
/// fn my_config() -> Dial9Config {
///     Dial9Config::builder()
///         .on_disk_buffer("/tmp/trace.bin")
///         .max_file_size(1024 * 1024)
///         .max_total_size(16 * 1024 * 1024)
///         .build()
///         .expect("config build failed")
/// }
///
/// #[dial9_tokio_telemetry::main(config = my_config)]
/// async fn main() {
///     let handle = Dial9TokioHandle::current();
///     handle
///         .spawn(async { /* instrumented sub-task */ })
///         .await
///         .unwrap();
/// }
/// ```
///
/// Using an inline closure:
///
/// ```rust,ignore
/// #[dial9_tokio_telemetry::main(config = || {
///     Dial9Config::builder()
///         .on_disk_buffer("/tmp/trace.bin")
///         .max_file_size(1024 * 1024)
///         .max_total_size(16 * 1024 * 1024)
///         .build()
///         .expect("config build failed")
/// })]
/// async fn main() {
///     /* ... */
/// }
/// ```
///
/// Lenient (telemetry is best-effort; falls back to a plain tokio
/// runtime if writer setup fails):
///
/// ```rust,ignore
/// #[dial9_tokio_telemetry::main(config = || {
///     Dial9Config::builder()
///         .on_disk_buffer("/tmp/trace.bin")
///         .max_file_size(1024 * 1024)
///         .max_total_size(16 * 1024 * 1024)
///         .build_or_disabled()
/// })]
/// async fn main() {
///     /* ... */
/// }
/// ```
///
/// Disabled (no telemetry, plain tokio runtime — useful for toggling
/// dial9 off via a feature flag or env var without removing the macro):
///
/// ```rust,ignore
/// #[dial9_tokio_telemetry::main(config = || {
///     Dial9Config::builder()
///         .on_disk_buffer("/tmp/trace.bin")
///         .enabled(false)
///         .build()
///         .expect("config build failed")
/// })]
/// async fn main() {
///     /* ... */
/// }
/// ```
///
/// In-memory writer (no telemetry on disk), select it with `.in_memory_buffer()`.
///
/// ```rust,ignore
/// #[dial9_tokio_telemetry::main(config = || {
///     Dial9Config::builder()
///         .in_memory_buffer()
///         .max_total_size(16 * 1024 * 1024)
///         .with_runtime(|r| r.with_custom_pipeline(|p| p.pipe(MyUploader)))
///         .build()
///         .expect("config build failed")
/// })]
/// async fn main() {
///     /* ... */
/// }
/// ```
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as MainArgs);
    let input = parse_macro_input!(item as ItemFn);

    match expand_main(args, input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn expand(attr: TokenStream2, item: TokenStream2) -> String {
        let args: MainArgs = syn::parse2(attr).expect("failed to parse args");
        let input: ItemFn = syn::parse2(item).expect("failed to parse fn");
        let expanded = expand_main(args, input).expect("expansion failed");
        let file = syn::parse2(expanded).expect("failed to parse expansion");
        prettyplease::unparse(&file)
    }

    #[test]
    fn expand_basic() {
        let output = expand(
            quote! { config = my_config },
            quote! {
                async fn main() {
                    do_work().await;
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn expand_with_return_type() {
        let output = expand(
            quote! { config = my_config },
            quote! {
                async fn main() -> Result<(), Box<dyn std::error::Error>> {
                    do_work().await?;
                    Ok(())
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn expand_with_attributes() {
        let output = expand(
            quote! { config = my_config },
            quote! {
                #[allow(unused)]
                async fn main() {
                    let _ = 42;
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    fn expand_err(attr: TokenStream2, item: TokenStream2) -> String {
        let args: MainArgs = syn::parse2(attr).expect("failed to parse args");
        let input: ItemFn = syn::parse2(item).expect("failed to parse fn");
        expand_main(args, input)
            .expect_err("expected error")
            .to_string()
    }

    #[test]
    fn error_with_arguments() {
        let msg = expand_err(
            quote! { config = my_config },
            quote! { async fn main(port: u16) {} },
        );
        assert!(
            msg.contains("does not support function arguments"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_with_generics() {
        let msg = expand_err(
            quote! { config = my_config },
            quote! { async fn main<T>() {} },
        );
        assert!(
            msg.contains("does not support generics"),
            "unexpected error: {msg}"
        );
    }

    fn parse_args_err(attr: TokenStream2) -> String {
        match syn::parse2::<MainArgs>(attr) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected parse error"),
        }
    }

    #[test]
    fn error_empty_args() {
        let msg = parse_args_err(quote! {});
        assert!(
            msg.contains("missing required `config`"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_wrong_arg_name() {
        let msg = parse_args_err(quote! { foo = bar });
        assert!(
            msg.contains("missing required `config`"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_config_with_args() {
        let msg = parse_args_err(quote! { config = my_config(arg) });
        assert!(msg.contains("zero-argument"), "unexpected error: {msg}");
    }

    #[test]
    fn error_config_trailing_tokens() {
        let msg = parse_args_err(quote! { config = my_config, extra = stuff });
        assert!(msg.contains("zero-argument"), "unexpected error: {msg}");
    }

    #[test]
    fn expand_with_inline_closure() {
        let output = expand(
            quote! { config = || my_config() },
            quote! {
                async fn main() {
                    do_work().await;
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn expand_with_move_closure() {
        let output = expand(
            quote! { config = move || my_config() },
            quote! {
                async fn main() {
                    do_work().await;
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn error_closure_with_args() {
        let msg = parse_args_err(quote! { config = |x| my_config() });
        assert!(msg.contains("zero-argument"), "unexpected error: {msg}");
    }

    #[test]
    fn error_not_async() {
        let args: MainArgs =
            syn::parse2(quote! { config = my_config }).expect("failed to parse args");
        let input: ItemFn = syn::parse2(quote! {
            fn main() {}
        })
        .expect("failed to parse fn");
        let err = expand_main(args, input).expect_err("expected error for non-async fn");
        let msg = err.to_string();
        assert!(msg.contains("async"), "error should mention async: {msg}");
    }
}
