//! Derive macro for `dial9_trace_format::TraceEvent`.
//!
//! See [`derive_trace_event`] for the supported `#[traceevent(...)]`
//! attributes.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

/// Unit values accepted by `#[traceevent(unit = "...")]`. Must stay in sync
/// with the viewer's `formatFieldValue` (dial9-viewer/ui/format.js).
const SUPPORTED_UNITS: &[&str] = &["ns", "us", "ms", "s", "bytes"];

/// The `#[traceevent(...)]` keys a field may carry.
#[derive(Default)]
struct FieldAttrs {
    /// `timestamp`: this field is the event timestamp (header, not a column).
    timestamp: bool,
    /// `unit = "..."`: rendering unit for this field.
    unit: Option<syn::LitStr>,
}

/// Parse one field's `#[traceevent(...)]` keys. Malformed or unknown keys are
/// compile errors rather than being silently ignored.
fn parse_field_attrs(field: &syn::Field) -> Result<FieldAttrs, syn::Error> {
    let mut parsed = FieldAttrs::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("traceevent") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("timestamp") {
                parsed.timestamp = true;
            } else if meta.path.is_ident("unit") {
                parsed.unit = Some(meta.value()?.parse::<syn::LitStr>()?);
            } else {
                return Err(meta.error(
                    "unrecognized `traceevent` field attribute; expected `timestamp` or \
                     `unit = \"...\"`",
                ));
            }
            Ok(())
        })?;
    }
    Ok(parsed)
}

fn derive_trace_event_impl(input: DeriveInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let name = &input.ident;

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("TraceEvent only supports named fields"),
        },
        _ => panic!("TraceEvent can only be derived for structs"),
    };

    // Parse struct-level attributes:
    // - `wire_slot`: opt this type into the encoder's inline fast path (a global
    //   slot doubling as wire id). Off by default.
    // - `name = <expr>`: override the wire event name (defaults to the struct
    //   name). Accepts any `&'static str` expression, not just a string literal,
    //   so callers can build a per-call-site-unique name, e.g.
    //   `concat!("SpanEnter:", file!(), ":", line!())`. Used to give generated
    //   structs a name the viewer recognizes (e.g. `"SpanEnter:..."`).
    let mut wire_slot = false;
    let mut name_override: Option<syn::Expr> = None;
    for attr in &input.attrs {
        if attr.path().is_ident("traceevent") {
            // Propagated, not swallowed: a malformed attribute (e.g. `name`
            // without a value) must be a compile error, not silently ignored.
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("wire_slot") {
                    wire_slot = true;
                } else if meta.path.is_ident("name") {
                    name_override = Some(meta.value()?.parse::<syn::Expr>()?);
                } else {
                    return Err(meta.error(
                        "unrecognized `traceevent` attribute; expected `wire_slot` or `name = ...`",
                    ));
                }
                Ok(())
            })?;
        }
    }
    // The wire event name expression returned by `event_name()`: either the
    // `name = ...` override (evaluated at the override's call site, so builtins
    // like `file!()`/`line!()` resolve there) or the struct name as a literal.
    let event_name_expr = match &name_override {
        Some(expr) => quote! { #expr },
        None => {
            let name_str = name.to_string();
            quote! { #name_str }
        }
    };

    // Every key of a field's `#[traceevent(...)]` is parsed in one pass: the
    // callback must consume each key's value, so a pass that recognized only
    // some keys would choke on the ones it skipped.
    let field_attrs = fields
        .iter()
        .map(parse_field_attrs)
        .collect::<Result<Vec<_>, _>>()?;

    // Find the field marked with #[traceevent(timestamp)]
    let mut timestamp_field_name = None;
    for (field, attrs) in fields.iter().zip(&field_attrs) {
        if attrs.timestamp {
            timestamp_field_name = Some(field.ident.as_ref().unwrap().clone());
        }
    }

    let mut field_def_tokens = Vec::new();
    let mut encode_tokens = Vec::new();
    let mut annotation_tokens = Vec::new();

    for (field, attrs) in fields.iter().zip(&field_attrs) {
        let field_name = field.ident.as_ref().unwrap();
        let ty = &field.ty;

        // `unit = "..."` is emitted as a "unit" schema annotation so viewers can
        // render the field in that unit.
        let unit = attrs.unit.clone();

        // Skip the timestamp field in schema/encode — it's in the event header
        if timestamp_field_name.as_ref() == Some(field_name) {
            if let Some(unit) = unit {
                return Err(syn::Error::new_spanned(
                    &unit,
                    "the timestamp field cannot carry a unit annotation: it is encoded in the \
                     event header (always nanoseconds), not as a schema field",
                ));
            }
            continue;
        }
        if let Some(unit) = unit {
            if !SUPPORTED_UNITS.contains(&unit.value().as_str()) {
                return Err(syn::Error::new_spanned(
                    &unit,
                    format!(
                        "unsupported unit \"{}\"; supported units: {}",
                        unit.value(),
                        SUPPORTED_UNITS.join(", ")
                    ),
                ));
            }
            // field_index matches the position in field_defs(), which
            // excludes the timestamp field.
            let idx = field_def_tokens.len() as u16;
            annotation_tokens.push(quote! {
                ::dial9_trace_format::schema::FieldAnnotation::new(#idx, "unit", #unit)
            });
        }

        let field_name_str = field_name.to_string();
        field_def_tokens.push(quote! {
            ::dial9_trace_format::schema::FieldDef::new(
                #field_name_str,
                <#ty as ::dial9_trace_format::TraceField>::field_type(),
            )
        });
        encode_tokens.push(quote! {
            <#ty as ::dial9_trace_format::TraceField>::encode(&self.#field_name, enc)?;
        });
    }

    let timestamp_impl = if let Some(ref ts_field) = timestamp_field_name {
        quote! {
            fn timestamp(&self) -> u64 { self.#ts_field }
        }
    } else {
        panic!("TraceEvent requires a field marked with #[traceevent(timestamp)]");
    };

    // `#[traceevent(wire_slot)]` types override `type_slot()`. Without it
    // the trait default returns 0 and the encoder uses the dynamic path.
    let type_slot_impl = if wire_slot {
        quote! {
            fn type_slot() -> u16 {
                static SLOT: ::std::sync::atomic::AtomicU16 =
                    ::std::sync::atomic::AtomicU16::new(0);
                let cached = SLOT.load(::std::sync::atomic::Ordering::Relaxed);
                if cached != 0 {
                    return cached;
                }
                let new = ::dial9_trace_format::__NEXT_TYPE_SLOT
                    .fetch_add(1, ::std::sync::atomic::Ordering::Relaxed);
                match SLOT.compare_exchange(
                    0,
                    new,
                    ::std::sync::atomic::Ordering::Relaxed,
                    ::std::sync::atomic::Ordering::Relaxed,
                ) {
                    Ok(_) => new,
                    Err(existing) => existing,
                }
            }
        }
    } else {
        quote! {}
    };

    // Only override the trait-default schema_entry() when a field carries an
    // annotation; the default builds the same entry with no annotations.
    let schema_entry_impl = if annotation_tokens.is_empty() {
        quote! {}
    } else {
        quote! {
            fn schema_entry() -> ::dial9_trace_format::schema::SchemaEntry {
                ::dial9_trace_format::schema::SchemaEntry::with_annotations(
                    Self::event_name(),
                    Self::has_timestamp(),
                    Self::field_defs(),
                    vec![#(#annotation_tokens),*],
                )
            }
        }
    };

    // Propagate any generics/lifetimes on the struct onto the impl, so the
    // derive works on generic event structs (e.g. an event that borrows
    // typed, generically-parameterized fields).
    //
    // Caveat for type parameters: `event_name()` is per *type*, not per
    // monomorphization, so all instantiations of a generic event share one wire
    // schema. That is sound only when a name maps to a single set of field
    // types, which is how `dial9_span!` uses it: each call site declares its
    // own struct with a call-site-unique `name = ...`, so it is instantiated
    // exactly once. Two instantiations with different field types under one
    // name would register conflicting schemas for that name.
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        // The generic params may be named after user fields (e.g. the ad-hoc
        // span macro uses the field ident as its type param), so silence the
        // casing lints on the generated impl.
        #[allow(non_camel_case_types, non_snake_case)]
        impl #impl_generics ::dial9_trace_format::TraceEvent for #name #ty_generics #where_clause {
            fn event_name() -> &'static str { #event_name_expr }
            #type_slot_impl
            fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
                vec![#(#field_def_tokens),*]
            }
            #schema_entry_impl
            #timestamp_impl
            fn encode_fields<W: ::std::io::Write>(&self, enc: &mut ::dial9_trace_format::EventEncoder<'_, W>) -> ::std::io::Result<()> {
                #(#encode_tokens)*
                Ok(())
            }
        }
    })
}

/// Derives `dial9_trace_format::TraceEvent` for a struct with named fields.
///
/// Supported attributes:
///
/// - `#[traceevent(timestamp)]` (field, required on exactly one `u64` field):
///   marks the event timestamp. It is encoded as a packed delta in the event
///   header, not as a regular field.
/// - `#[traceevent(wire_slot)]` (struct): opts the type into the encoder's
///   inline fast path by claiming a static wire-ID slot.
/// - `#[traceevent(name = <expr>)]` (struct): overrides the wire event name
///   (defaults to the struct name). Accepts any `&'static str` expression, not
///   just a string literal, so callers can build a per-call-site-unique name —
///   e.g. `concat!("SpanEnter:", file!(), ":", line!())`. Useful for generated
///   structs that need a name the viewer recognizes (e.g. `"SpanEnter:..."`),
///   which cannot be a valid Rust identifier.
/// - `#[traceevent(unit = "...")]` (field): attaches a `unit` schema
///   annotation so viewers render the field in that unit. Supported values:
///   `"ns"`, `"us"`, `"ms"`, `"s"`, `"bytes"`. Any other value is a compile
///   error, as is placing `unit` on the timestamp field (the timestamp is
///   encoded in the event header and is always nanoseconds).
///
/// A malformed or unrecognized `traceevent` key is a compile error.
///
/// # Generic event structs
///
/// Lifetimes and type parameters are carried onto the generated impl. Note that
/// the wire event name is per type, not per monomorphization: a generic event
/// must therefore have a single instantiation per `name`, as generated
/// per-call-site structs do. Instantiating one generic event with different
/// field types under the same name registers conflicting schemas for that name.
///
/// ```ignore
/// #[derive(TraceEvent)]
/// struct RequestCompleted {
///     #[traceevent(timestamp)]
///     timestamp_ns: u64,
///     #[traceevent(unit = "us")]
///     latency_us: u64,
///     status_code: u32,
/// }
/// ```
#[proc_macro_derive(TraceEvent, attributes(traceevent))]
pub fn derive_trace_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match derive_trace_event_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use quote::quote;

    fn expand_to_string(input: proc_macro2::TokenStream) -> String {
        let input: DeriveInput = syn::parse2(input).unwrap();
        let output = derive_trace_event_impl(input).expect("expansion failed");
        match syn::parse2::<syn::File>(output.clone()) {
            Ok(file) => prettyplease::unparse(&file),
            Err(_) => output.to_string(),
        }
    }

    fn expand_err(input: proc_macro2::TokenStream) -> syn::Error {
        let input: DeriveInput = syn::parse2(input).unwrap();
        derive_trace_event_impl(input).expect_err("expansion should fail")
    }

    #[test]
    fn simple_event() {
        assert_snapshot!(expand_to_string(quote! {
            struct SimpleEvent {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                value: u32,
            }
        }));
    }

    #[test]
    fn empty_event() {
        assert_snapshot!(expand_to_string(quote! {
            struct EmptyEvent {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
            }
        }));
    }

    #[test]
    fn all_field_types() {
        assert_snapshot!(expand_to_string(quote! {
            struct AllFieldTypes {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                a_u8: u8,
                b_u16: u16,
                c_u32: u32,
                d_u64: u64,
                e_i64: i64,
                f_f64: f64,
                g_bool: bool,
                h_string: String,
                i_bytes: Vec<u8>,
                j_interned: InternedString,
                k_frames: StackFrames,
                l_map: Vec<(String, String)>,
            }
        }));
    }

    #[test]
    fn doc_comments_copied_to_ref_fields() {
        assert_snapshot!(expand_to_string(quote! {
            /// Root documentation
            struct DocEvent {
                #[traceevent(timestamp)]
                /// Event timestamp in nanoseconds.
                timestamp_ns: u64,
                /// The worker thread ID.
                worker_id: u64,
                /// Number of items in the local queue.
                local_queue: u8,
            }
        }));
    }

    #[test]
    fn wire_slot_event() {
        assert_snapshot!(expand_to_string(quote! {
            #[traceevent(wire_slot)]
            struct WireSlotEvent {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                value: u32,
            }
        }));
    }

    #[test]
    fn unit_attribute() {
        assert_snapshot!(expand_to_string(quote! {
            struct ResourceUsage {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(unit = "ns")]
                user_cpu_ns: u64,
                minor_faults: u64,
                #[traceevent(unit = "bytes")]
                max_rss_bytes: u64,
            }
        }));
    }

    #[test]
    fn invalid_unit_rejected() {
        let err = expand_err(quote! {
            struct BadUnit {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(unit = "nss")]
                value: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "unsupported unit \"nss\"; supported units: ns, us, ms, s, bytes"
        );
    }

    #[test]
    fn unit_on_timestamp_rejected() {
        let err = expand_err(quote! {
            struct TimestampUnit {
                #[traceevent(timestamp)]
                #[traceevent(unit = "ns")]
                timestamp_ns: u64,
                value: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "the timestamp field cannot carry a unit annotation: it is encoded in the \
             event header (always nanoseconds), not as a schema field"
        );
    }

    #[test]
    fn mu_char_unit_rejected() {
        let err = expand_err(quote! {
            struct MuUnit {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(unit = "µs")]
                latency: u64,
            }
        });
        assert!(err.to_string().contains("unsupported unit \"µs\""));
    }

    /// A generic event carries its params onto the impl; the schema name comes
    /// from the per-instantiation `name = ...`, not from the type parameters.
    #[test]
    fn generic_event() {
        assert_snapshot!(expand_to_string(quote! {
            #[traceevent(name = concat!("SpanEnter:", file!(), ":", line!()))]
            struct GenericEvent<'a, T: TraceField + Clone + 'static> {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                name: &'a str,
                value: T,
            }
        }));
    }

    #[test]
    fn malformed_name_rejected() {
        let err = expand_err(quote! {
            #[traceevent(name)]
            struct MalformedName {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
            }
        });
        assert!(
            err.to_string().contains("expected `=`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unknown_struct_attribute_rejected() {
        let err = expand_err(quote! {
            #[traceevent(wire_slots)]
            struct Typo {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "unrecognized `traceevent` attribute; expected `wire_slot` or `name = ...`"
        );
    }

    #[test]
    fn unknown_field_attribute_rejected() {
        let err = expand_err(quote! {
            struct Typo {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(units = "ns")]
                value: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "unrecognized `traceevent` field attribute; expected `timestamp` or `unit = \"...\"`"
        );
    }

    #[test]
    fn timestamp_attribute() {
        assert_snapshot!(expand_to_string(quote! {
            struct PollStart {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                worker_id: u64,
                task_id: u64,
            }
        }));
    }
}
