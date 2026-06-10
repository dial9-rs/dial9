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

fn derive_trace_event_impl(input: DeriveInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let name = &input.ident;
    let name_str = name.to_string();

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("TraceEvent only supports named fields"),
        },
        _ => panic!("TraceEvent can only be derived for structs"),
    };

    // Parse struct-level #[traceevent(wire_slot)]: opt this type into the
    // encoder's inline fast path (a global slot doubling as wire id). Off by
    // default.
    let mut wire_slot = false;
    for attr in &input.attrs {
        if attr.path().is_ident("traceevent") {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("wire_slot") {
                    wire_slot = true;
                }
                Ok(())
            });
        }
    }

    // Find the field marked with #[traceevent(timestamp)]
    let mut timestamp_field_name = None;
    for field in fields.iter() {
        for attr in &field.attrs {
            if attr.path().is_ident("traceevent") {
                let _ = attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("timestamp") {
                        timestamp_field_name = Some(field.ident.as_ref().unwrap().clone());
                    }
                    Ok(())
                });
            }
        }
    }

    let mut field_def_tokens = Vec::new();
    let mut encode_tokens = Vec::new();
    let mut annotation_tokens = Vec::new();

    for field in fields.iter() {
        let field_name = field.ident.as_ref().unwrap();
        let ty = &field.ty;

        // Parse #[traceevent(unit = "...")]: emitted as a "unit"
        // schema annotation so viewers can render the field in that unit.
        let mut unit: Option<syn::LitStr> = None;
        for attr in &field.attrs {
            if attr.path().is_ident("traceevent") {
                let _ = attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("unit") {
                        unit = Some(meta.value()?.parse::<syn::LitStr>()?);
                    }
                    Ok(())
                });
            }
        }

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

    Ok(quote! {
        impl ::dial9_trace_format::TraceEvent for #name {
            fn event_name() -> &'static str { #name_str }
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
/// - `#[traceevent(unit = "...")]` (field): attaches a `unit` schema
///   annotation so viewers render the field in that unit. Supported values:
///   `"ns"`, `"us"`, `"ms"`, `"s"`, `"bytes"`. Any other value is a compile
///   error, as is placing `unit` on the timestamp field (the timestamp is
///   encoded in the event header and is always nanoseconds).
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
