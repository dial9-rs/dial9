use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprClosure, ItemFn, Meta, Path, Token, parse_macro_input};

enum ConfigSource {
    Path(Path),
    Closure(ExprClosure),
}

/// Parsed `#[dial9::main(..)]` arguments.
struct MainArgs {
    /// Zero-argument function/closure returning `io::Result<dial9::AttachedRuntime>`.
    config: ConfigSource,
    /// Graceful-shutdown deadline (a `Duration` expression). Defaults to 1s.
    graceful_shutdown: Option<Expr>,
    /// Skip the graceful drain entirely.
    disable_graceful_shutdown: bool,
}

const MISSING_CONFIG_HELP: &str = "missing required `config` argument, e.g.\n  \
                           #[dial9::main(config = dial9::recorder_from_env)]\n\
                           or with an inline closure:\n  \
                           #[dial9::main(config = || {\n    \
                           let rec = dial9::recorder(writer).build();\n    \
                           let mut b = tokio::runtime::Builder::new_multi_thread();\n    \
                           b.enable_all();\n    \
                           let rt = rec.handle().attach_tokio_runtime(b, TokioAttachOptions::default())?;\n    \
                           Ok((rec, rt))\n  \
                           })]";

const CONFIG_MUST_BE_ZERO_ARG_HELP: &str = "`config` must be a zero-argument function path or a zero-argument closure returning `std::io::Result<dial9::AttachedRuntime>`, e.g.\n  \
                           #[dial9::main(config = my_config_fn)]\n\
                           or with an inline closure:\n  \
                           #[dial9::main(config = || {\n    \
                           let rec = dial9::recorder(writer).build();\n    \
                           let mut b = tokio::runtime::Builder::new_multi_thread();\n    \
                           b.enable_all();\n    \
                           let rt = rec.handle().attach_tokio_runtime(b, TokioAttachOptions::default())?;\n    \
                           Ok((rec, rt))\n  \
                           })]";

/// `config`'s value: a bare function path or a zero-argument closure. Anything
/// else (a call, a literal) is the mistake `CONFIG_MUST_BE_ZERO_ARG_HELP` names.
fn config_from_expr(expr: &Expr) -> syn::Result<ConfigSource> {
    match expr {
        Expr::Path(path) => Ok(ConfigSource::Path(path.path.clone())),
        Expr::Closure(closure) if closure.inputs.is_empty() => {
            Ok(ConfigSource::Closure(closure.clone()))
        }
        other => Err(syn::Error::new_spanned(other, CONFIG_MUST_BE_ZERO_ARG_HELP)),
    }
}

/// Store `value`, rejecting a second occurrence of the same argument.
fn set_once<T>(slot: &mut Option<T>, value: T, name: &str, span: Span) -> syn::Result<()> {
    if slot.is_some() {
        return Err(syn::Error::new(
            span,
            format!("`{name}` set multiple times in `#[dial9::main]`"),
        ));
    }
    *slot = Some(value);
    Ok(())
}

/// The `= <expr>` of a `name = value` argument, or an error naming what was
/// written instead.
fn require_value<'a>(meta: &'a Meta, name: &str) -> syn::Result<&'a Expr> {
    match meta {
        Meta::NameValue(nv) => Ok(&nv.value),
        other => Err(syn::Error::new_spanned(
            other,
            format!("`{name}` takes a value: `{name} = ...`"),
        )),
    }
}

impl Parse for MainArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error(MISSING_CONFIG_HELP));
        }

        let mut config = None;
        let mut graceful_shutdown = None;
        let mut disable_graceful_shutdown = None;

        for meta in Punctuated::<Meta, Token![,]>::parse_terminated(input)? {
            let span = meta.span();
            let name = meta
                .path()
                .get_ident()
                .map(syn::Ident::to_string)
                .unwrap_or_default();
            match name.as_str() {
                "config" => set_once(
                    &mut config,
                    config_from_expr(require_value(&meta, "config")?)?,
                    "config",
                    span,
                )?,
                "graceful_shutdown" => set_once(
                    &mut graceful_shutdown,
                    require_value(&meta, "graceful_shutdown")?.clone(),
                    "graceful_shutdown",
                    span,
                )?,
                "disable_graceful_shutdown" => {
                    if !matches!(meta, Meta::Path(_)) {
                        return Err(syn::Error::new(
                            span,
                            "`disable_graceful_shutdown` is a flag and takes no value",
                        ));
                    }
                    set_once(
                        &mut disable_graceful_shutdown,
                        (),
                        "disable_graceful_shutdown",
                        span,
                    )?;
                }
                other => {
                    return Err(syn::Error::new(
                        meta.path().span(),
                        format!(
                            "unknown `#[dial9::main]` argument `{other}`; expected `config`, \
                             `graceful_shutdown`, or `disable_graceful_shutdown`"
                        ),
                    ));
                }
            }
        }

        let Some(config) = config else {
            return Err(syn::Error::new(Span::call_site(), MISSING_CONFIG_HELP));
        };
        if let (Some(gs), Some(())) = (&graceful_shutdown, &disable_graceful_shutdown) {
            return Err(syn::Error::new(
                gs.span(),
                "`disable_graceful_shutdown` and `graceful_shutdown` cannot both be set",
            ));
        }
        Ok(MainArgs {
            config,
            graceful_shutdown,
            disable_graceful_shutdown: disable_graceful_shutdown.is_some(),
        })
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
            "#[dial9::main] does not support function arguments",
        ));
    }

    if !input.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.sig.generics,
            "#[dial9::main] does not support generics",
        ));
    }

    if input.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &input.sig.generics.where_clause,
            "#[dial9::main] does not support where clauses",
        ));
    }

    let config_call = match &args.config {
        ConfigSource::Path(p) => quote! { #p() },
        ConfigSource::Closure(c) => quote! { (#c)() },
    };

    let shutdown_stmt = if args.disable_graceful_shutdown {
        quote! { drop(__dial9_recorder); }
    } else {
        let timeout = args
            .graceful_shutdown
            .as_ref()
            .map(|e| quote! { #e })
            .unwrap_or_else(|| quote! { ::std::time::Duration::from_secs(1) });
        quote! { __dial9_recorder.graceful_shutdown(#timeout); }
    };

    let attrs = &input.attrs;
    let vis = &input.vis;
    let name = &input.sig.ident;
    let ret = &input.sig.output;
    let body_stmts = &input.block.stmts;

    // `config` yields the recorder (already recording, unless the config paused
    // it) and its instrumented runtime. Run the body through `dial9::block_on`,
    // which spawns it as a task: polled directly under `Runtime::block_on` it
    // would be invisible to the poll hooks. Then drop the runtime so workers
    // flush, and drain.
    Ok(quote! {
        #(#attrs)*
        #vis fn #name() #ret {
            let __dial9_config_result: ::std::io::Result<::dial9::AttachedRuntime> = #config_call;
            let (__dial9_recorder, __dial9_rt) =
                __dial9_config_result.expect("dial9::main: config failed");
            let __dial9_out = ::dial9::block_on(&__dial9_rt, async move { #(#body_stmts)* });
            drop(__dial9_rt);
            #shutdown_stmt
            __dial9_out
        }
    })
}

/// Instrument an async main function with dial9 telemetry.
///
/// This macro is a **replacement** for `#[tokio::main]`, not a complement —
/// do not use both attributes on the same function. Your `config` yields a [`dial9::Recorder`](../dial9/struct.Recorder.html) and its
/// instrumented `tokio::runtime::Runtime`; the macro runs the body on that
/// runtime as a spawned task via [`dial9::block_on`](../dial9/fn.block_on.html) (polled directly under
/// `Runtime::block_on` it would be invisible to the poll hooks), then drops the
/// runtime (so workers flush) and drains the recorder.
///
/// Spawn instrumented sub-tasks from the body with `dial9::spawn`.
///
/// # Arguments
///
/// * `config` — a zero-argument function path or closure returning
///   `std::io::Result<`[`dial9::AttachedRuntime`](../dial9/type.AttachedRuntime.html)`>`: a recorder paired with a
///   runtime attached to it
///   via [`Dial9HandleTokioExt::attach_tokio_runtime`](../dial9/trait.Dial9HandleTokioExt.html#tymethod.attach_tokio_runtime).
///   The macro panics if it is an `Err`. Use [`dial9::recorder_from_env`](../dial9/fn.recorder_from_env.html) for
///   the env-driven setup. Required.
/// * `graceful_shutdown` — the drain deadline (a `Duration`); defaults to 1s.
/// * `disable_graceful_shutdown` — skip the drain; the recorder is just dropped.
///
/// # Graceful shutdown
///
/// After the async body returns, the macro drops the runtime (so Tokio worker
/// threads exit and flush their thread-local buffers) and then drains the
/// recorder's background worker so the final segment is symbolized, compressed,
/// and uploaded before the process exits.
///
/// The implicit drain only runs when the body returns normally. If the body
/// panics, the panic propagates and the recorder's `Drop` still flushes and
/// seals the final segment, but the background worker is not drained — so a
/// panicking program may not symbolize or upload its last segment.
///
/// # Examples
///
/// From the environment (the common production path):
///
/// ```no_run
/// #[dial9::main(config = dial9::recorder_from_env)]
/// async fn main() {
///     dial9::spawn(async { /* instrumented sub-task */ }).await.unwrap();
/// }
/// ```
///
/// A named config that builds its own recorder and runtime:
///
/// ```no_run
/// use std::io;
/// use dial9::{AttachedRuntime, Dial9HandleTokioExt, DiskBuffer, TokioAttachOptions};
///
/// fn my_config() -> io::Result<AttachedRuntime> {
///     let writer = DiskBuffer::builder()
///         .base_path("/tmp/traces")
///         .max_total_size(16 * 1024 * 1024)
///         .build()
///         .expect("writer build failed");
///     let recorder = dial9::recorder(writer).build();
///
///     let mut builder = tokio::runtime::Builder::new_multi_thread();
///     builder.enable_all().worker_threads(4);
///     let runtime = recorder
///         .handle()
///         .attach_tokio_runtime(builder, TokioAttachOptions::default())?;
///
///     Ok((recorder, runtime))
/// }
///
/// #[dial9::main(config = my_config, graceful_shutdown = std::time::Duration::from_secs(5))]
/// async fn main() {
///     /* ... */
/// }
/// ```
///
/// Disabled (no telemetry, plain tokio runtime — useful for toggling
/// dial9 off via a feature flag or env var without removing the macro):
///
/// ```no_run
/// use dial9::{Dial9HandleTokioExt, TokioAttachOptions};
///
/// #[dial9::main(config = || {
///     let recorder = dial9::recorder_disabled();
///     let mut builder = tokio::runtime::Builder::new_multi_thread();
///     builder.enable_all();
///     let runtime = recorder
///         .handle()
///         .attach_tokio_runtime(builder, TokioAttachOptions::default())?;
///     Ok((recorder, runtime))
/// })]
/// async fn main() {
///     /* ... */
/// }
/// ```
///
/// In-memory writer (nothing on local disk). With no disk writeback, pair it
/// with a pipeline that ships the buffered segments somewhere — e.g.
/// `.with_s3_uploader(..)` or `.with_custom_pipeline(..)`.
///
/// ```no_run
/// use dial9::{Dial9HandleTokioExt, TokioAttachOptions};
///
/// #[dial9::main(config = || {
///     let writer = dial9::MemoryBuffer::builder()
///         .max_total_size(16 * 1024 * 1024)
///         .build()
///         .expect("writer build failed");
///     let recorder = dial9::recorder(writer).build();
///     let mut builder = tokio::runtime::Builder::new_multi_thread();
///     builder.enable_all();
///     let runtime = recorder
///         .handle()
///         .attach_tokio_runtime(builder, TokioAttachOptions::default())?;
///     Ok((recorder, runtime))
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

    /// `graceful_shutdown = <expr>` replaces the default 1s drain deadline.
    #[test]
    fn expand_with_graceful_shutdown_timeout() {
        let output = expand(
            quote! { config = my_config, graceful_shutdown = std::time::Duration::from_secs(7) },
            quote! {
                async fn main() {
                    do_work().await;
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    /// `disable_graceful_shutdown` drops the recorder instead of draining it.
    #[test]
    fn expand_with_disable_graceful_shutdown() {
        let output = expand(
            quote! { config = my_config, disable_graceful_shutdown },
            quote! {
                async fn main() {
                    do_work().await;
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
            msg.contains("unknown `#[dial9::main]` argument `foo`"),
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
        assert!(
            msg.contains("unknown `#[dial9::main]` argument `extra`"),
            "unexpected error: {msg}"
        );
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
    fn error_graceful_shutdown_and_disable_both_set() {
        let msg = parse_args_err(
            quote! { config = my_config, graceful_shutdown = d, disable_graceful_shutdown },
        );
        assert!(
            msg.contains("cannot both be set"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_closure_with_args() {
        let msg = parse_args_err(quote! { config = |x| my_config() });
        assert!(msg.contains("zero-argument"), "unexpected error: {msg}");
    }

    /// A repeated argument is a mistake, not a silent last-one-wins.
    #[test]
    fn error_duplicate_config() {
        let msg = parse_args_err(quote! { config = a, config = b });
        assert!(
            msg.contains("`config` set multiple times"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_duplicate_graceful_shutdown() {
        let msg = parse_args_err(
            quote! { config = my_config, graceful_shutdown = a, graceful_shutdown = b },
        );
        assert!(
            msg.contains("`graceful_shutdown` set multiple times"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_duplicate_disable_graceful_shutdown() {
        let msg = parse_args_err(
            quote! { config = my_config, disable_graceful_shutdown, disable_graceful_shutdown },
        );
        assert!(
            msg.contains("`disable_graceful_shutdown` set multiple times"),
            "unexpected error: {msg}"
        );
    }

    /// `disable_graceful_shutdown` is a bare flag; `= true` is not accepted.
    #[test]
    fn error_flag_with_value() {
        let msg = parse_args_err(quote! { config = my_config, disable_graceful_shutdown = true });
        assert!(
            msg.contains("is a flag and takes no value"),
            "unexpected error: {msg}"
        );
    }

    /// `config` without `= ...` names the omission rather than falling through
    /// to the unknown-argument arm.
    #[test]
    fn error_config_without_value() {
        let msg = parse_args_err(quote! { config });
        assert!(
            msg.contains("`config` takes a value"),
            "unexpected error: {msg}"
        );
    }

    /// A non-path, non-closure value is the same mistake as `config = f(arg)`.
    #[test]
    fn error_config_literal() {
        let msg = parse_args_err(quote! { config = 42 });
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
