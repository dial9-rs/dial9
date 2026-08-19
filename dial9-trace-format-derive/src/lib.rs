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

/// Metric interpretations accepted by `#[traceevent(kind = "...")]`. Must stay
/// in sync with the viewer's `FieldChartKind`.
const SUPPORTED_KINDS: &[&str] = &["gauge", "counter", "updown-counter"];

/// Annotation key for `#[traceevent(role = "...")]`. Mirrors
/// `dial9_core::schema_extensions::ROLE_KEY`, which this crate cannot depend on.
const ROLE_ANNOTATION_KEY: &str = "dial9.role";

/// Structural roles accepted by `#[traceevent(role = "...")]`. Mirrors the
/// vocabulary in `dial9_core::schema_extensions::roles`, which this crate cannot
/// depend on. An unrecognized role would silently decode as no role (turning a
/// span schema into `NotSpan`), so a typo is rejected at compile time.
const SUPPORTED_ROLES: &[&str] = &[
    "span.start",
    "span.duration",
    "span.name",
    "thread_id",
    "tokio.task_id",
    "tokio.worker_id",
];

/// The `#[traceevent(...)]` keys a field may carry.
#[derive(Default)]
struct FieldAttrs {
    /// `timestamp`: this field is the event timestamp (header, not a column).
    timestamp: bool,
    /// `name = "..."`: override this field's wire-schema name.
    name: Option<syn::LitStr>,
    /// `unit = "..."`: rendering unit for this field.
    unit: Option<syn::LitStr>,
    /// `role = "..."`: structural role for this field (`dial9.role`).
    role: Option<syn::LitStr>,
    /// `kind = "..."`: metric interpretation for this field.
    kind: Option<syn::LitStr>,
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
            } else if meta.path.is_ident("name") {
                parsed.name = Some(meta.value()?.parse::<syn::LitStr>()?);
            } else if meta.path.is_ident("unit") {
                parsed.unit = Some(meta.value()?.parse::<syn::LitStr>()?);
            } else if meta.path.is_ident("role") {
                parsed.role = Some(meta.value()?.parse::<syn::LitStr>()?);
            } else if meta.path.is_ident("kind") {
                parsed.kind = Some(meta.value()?.parse::<syn::LitStr>()?);
            } else {
                return Err(meta.error(
                    "unrecognized `traceevent` field attribute; expected `timestamp`, \
                     `name = \"...\"`, `unit = \"...\"`, `role = \"...\"` or `kind = \"...\"`",
                ));
            }
            Ok(())
        })?;
    }
    Ok(parsed)
}

fn derive_trace_event_impl(input: DeriveInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let name = &input.ident;

    // Support borrowed event structs like `Event<'a> { data: &'a str }`. We
    // allow at most one lifetime and no type/const parameters; generic event
    // schemas are not currently supported.
    if input.generics.type_params().next().is_some()
        || input.generics.const_params().next().is_some()
        || input.generics.lifetimes().count() > 1
    {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "TraceEvent supports at most one lifetime parameter and no type or const parameters",
        ));
    }
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

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
    let mut field_def_names = Vec::new();
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
            if let Some(name) = &attrs.name {
                return Err(syn::Error::new_spanned(
                    name,
                    "the timestamp field cannot have a wire name: it is encoded in the event \
                     header, not as a schema field",
                ));
            }
            if let Some(unit) = unit {
                return Err(syn::Error::new_spanned(
                    &unit,
                    "the timestamp field cannot carry a unit annotation: it is encoded in the \
                     event header (always nanoseconds), not as a schema field",
                ));
            }
            if let Some(role) = &attrs.role {
                return Err(syn::Error::new_spanned(
                    role,
                    "the timestamp field cannot carry a role annotation: it is encoded in the \
                     event header, not as a schema field",
                ));
            }
            if let Some(kind) = &attrs.kind {
                return Err(syn::Error::new_spanned(
                    kind,
                    "the timestamp field cannot carry a kind annotation: it is encoded in the \
                     event header, not as a schema field",
                ));
            }
            continue;
        }
        let field_name_lit = attrs.name.clone().unwrap_or_else(|| {
            syn::LitStr::new(&field_name.to_string(), proc_macro2::Span::call_site())
        });
        let field_name_value = field_name_lit.value();
        if field_def_names.contains(&field_name_value) {
            return Err(syn::Error::new_spanned(
                &field_name_lit,
                format!("duplicate trace event field name \"{field_name_value}\""),
            ));
        }
        field_def_names.push(field_name_value);
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
        if let Some(role) = &attrs.role {
            if !SUPPORTED_ROLES.contains(&role.value().as_str()) {
                return Err(syn::Error::new_spanned(
                    role,
                    format!(
                        "unsupported role \"{}\"; supported roles: {}",
                        role.value(),
                        SUPPORTED_ROLES.join(", ")
                    ),
                ));
            }
            let idx = field_def_tokens.len() as u16;
            annotation_tokens.push(quote! {
                ::dial9_trace_format::schema::FieldAnnotation::new(
                    #idx,
                    #ROLE_ANNOTATION_KEY,
                    #role,
                )
            });
        }
        // `kind = "..."` is emitted as a "kind" schema annotation telling the
        // viewer how to chart the field (gauge / counter / updown-counter).
        if let Some(kind) = &attrs.kind {
            if !SUPPORTED_KINDS.contains(&kind.value().as_str()) {
                return Err(syn::Error::new_spanned(
                    kind,
                    format!(
                        "unsupported kind \"{}\"; supported kinds: {}",
                        kind.value(),
                        SUPPORTED_KINDS.join(", ")
                    ),
                ));
            }
            let idx = field_def_tokens.len() as u16;
            annotation_tokens.push(quote! {
                ::dial9_trace_format::schema::FieldAnnotation::new(#idx, "kind", #kind)
            });
        }

        field_def_tokens.push(quote! {
            ::dial9_trace_format::schema::FieldDef::new(
                #field_name_lit,
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
                    Self::field_defs(),
                    vec![#(#annotation_tokens),*],
                )
            }
        }
    };

    Ok(quote! {
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
/// - `#[traceevent(name = "...")]` (field): overrides the field's wire-schema
///   name. This is useful for canonical names that are not valid Rust
///   identifiers, such as `"dial9.tokio.task_id"`.
/// - `#[traceevent(unit = "...")]` (field): attaches a `unit` schema
///   annotation so viewers render the field in that unit. Supported values:
///   `"ns"`, `"us"`, `"ms"`, `"s"`, `"bytes"`. Any other value is a compile
///   error, as is placing `unit` on the timestamp field (the timestamp is
///   encoded in the event header and is always nanoseconds).
///
/// - `#[traceevent(role = "...")]` (field): attaches a `dial9.role` schema
///   annotation, telling consumers what the field *is* structurally (e.g.
///   `"span.name"`). The vocabulary lives in
///   `dial9_core::schema_extensions::roles`; an unrecognized role is a compile
///   error (it would otherwise decode as no role).
/// - `#[traceevent(kind = "...")]` (field): attaches a `kind` schema annotation
///   telling the viewer how to chart the field. Supported values: `"gauge"`,
///   `"counter"`, `"updown-counter"`. Any other value is a compile error, as is
///   placing `kind` on the timestamp field.
///
/// A malformed or unrecognized `traceevent` key is a compile error. Only structs
/// with named fields and at most one lifetime parameter are supported; type and
/// const parameters are rejected.
///
/// # Example
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
    fn role_attribute() {
        assert_snapshot!(expand_to_string(quote! {
            struct SpanEnter {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(role = "span.name")]
                span_name: InternedString,
                #[traceevent(unit = "ns")]
                active_ns: u64,
            }
        }));
    }

    /// `name = <expr>` overrides `event_name()` with the given expression
    /// (evaluated at the caller's site), so a generated struct can build a
    /// per-call-site-unique name via `file!()`/`line!()`.
    #[test]
    fn name_attribute() {
        assert_snapshot!(expand_to_string(quote! {
            #[traceevent(name = concat!("SpanEnter:", file!(), ":", line!()))]
            struct Renamed {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                value: u64,
            }
        }));
    }

    #[test]
    fn field_name_attribute() {
        let expanded = expand_to_string(quote! {
            struct TaskEvent {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
                task_id: Option<u64>,
            }
        });
        let compact: String = expanded.split_whitespace().collect();
        assert!(
            compact.contains("FieldDef::new(\"dial9.tokio.task_id\","),
            "wire field override missing from expansion:\n{expanded}"
        );
        assert!(
            compact.contains("TraceField>::encode(&self.task_id,enc)?"),
            "encoding must still read the Rust field:\n{expanded}"
        );
    }

    #[test]
    fn duplicate_field_name_rejected() {
        let err = expand_err(quote! {
            struct DuplicateName {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(name = "value")]
                first: u64,
                value: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "duplicate trace event field name \"value\""
        );
    }

    #[test]
    fn field_name_on_timestamp_rejected() {
        let err = expand_err(quote! {
            struct NamedTimestamp {
                #[traceevent(timestamp, name = "timestamp")]
                timestamp_ns: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "the timestamp field cannot have a wire name: it is encoded in the event header, \
             not as a schema field"
        );
    }

    #[test]
    fn kind_attribute() {
        assert_snapshot!(expand_to_string(quote! {
            struct Metrics {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(unit = "ns", kind = "counter")]
                cpu_time_ns: u64,
                #[traceevent(kind = "gauge")]
                queue_depth: u64,
                #[traceevent(kind = "updown-counter")]
                active_requests: i64,
            }
        }));
    }

    #[test]
    fn invalid_kind_rejected() {
        let err = expand_err(quote! {
            struct BadKind {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(kind = "histogram")]
                value: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "unsupported kind \"histogram\"; supported kinds: gauge, counter, updown-counter"
        );
    }

    #[test]
    fn invalid_role_rejected() {
        let err = expand_err(quote! {
            struct BadRole {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                #[traceevent(role = "span.naem")]
                span_name: InternedString,
            }
        });
        assert_eq!(
            err.to_string(),
            "unsupported role \"span.naem\"; supported roles: span.start, span.duration, \
             span.name, thread_id, tokio.task_id, tokio.worker_id"
        );
    }

    #[test]
    fn kind_on_timestamp_rejected() {
        let err = expand_err(quote! {
            struct TimestampKind {
                #[traceevent(timestamp)]
                #[traceevent(kind = "counter")]
                timestamp_ns: u64,
                value: u64,
            }
        });
        assert_eq!(
            err.to_string(),
            "the timestamp field cannot carry a kind annotation: it is encoded in the \
             event header, not as a schema field"
        );
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
    fn borrowed_str_event() {
        assert_snapshot!(expand_to_string(quote! {
            struct BorrowedStr<'a> {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                path: &'a str,
            }
        }));
    }

    #[test]
    fn borrowed_bytes_event() {
        assert_snapshot!(expand_to_string(quote! {
            struct BorrowedBytes<'a> {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                body: &'a [u8],
            }
        }));
    }

    #[test]
    fn mixed_owned_and_borrowed() {
        assert_snapshot!(expand_to_string(quote! {
            struct Mixed<'a> {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                owned: String,
                borrowed: &'a str,
                count: u32,
            }
        }));
    }

    #[test]
    fn wire_slot_with_lifetime() {
        assert_snapshot!(expand_to_string(quote! {
            #[traceevent(wire_slot)]
            struct WireSlotBorrowed<'a> {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                data: &'a str,
            }
        }));
    }

    #[test]
    fn two_lifetimes_rejected() {
        let err = expand_err(quote! {
            struct TwoLifetimes<'a, 'b> {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                a: &'a str,
                b: &'b str,
            }
        });
        assert!(
            err.to_string().contains("at most one lifetime"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn type_param_rejected() {
        let err = expand_err(quote! {
            struct Generic<T> {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                value: T,
            }
        });
        assert!(
            err.to_string().contains("no type or const parameters"),
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
            "unrecognized `traceevent` field attribute; expected `timestamp`, `name = \"...\"`, \
             `unit = \"...\"`, `role = \"...\"` or `kind = \"...\"`"
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
