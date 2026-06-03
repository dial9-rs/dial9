use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

fn derive_trace_event_impl(input: DeriveInput) -> proc_macro2::TokenStream {
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

    for field in fields.iter() {
        let field_name = field.ident.as_ref().unwrap();
        let ty = &field.ty;

        // Skip the timestamp field in schema/encode — it's in the event header
        if timestamp_field_name.as_ref() == Some(field_name) {
            continue;
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

    quote! {
        impl ::dial9_trace_format::TraceEvent for #name {
            fn event_name() -> &'static str { #name_str }
            #type_slot_impl
            fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
                vec![#(#field_def_tokens),*]
            }
            #timestamp_impl
            fn encode_fields<W: ::std::io::Write>(&self, enc: &mut ::dial9_trace_format::EventEncoder<'_, W>) -> ::std::io::Result<()> {
                #(#encode_tokens)*
                Ok(())
            }
        }
    }
}

#[proc_macro_derive(TraceEvent, attributes(traceevent))]
pub fn derive_trace_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    TokenStream::from(derive_trace_event_impl(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use quote::quote;

    fn expand_to_string(input: proc_macro2::TokenStream) -> String {
        let input: DeriveInput = syn::parse2(input).unwrap();
        let output = derive_trace_event_impl(input);
        match syn::parse2::<syn::File>(output.clone()) {
            Ok(file) => prettyplease::unparse(&file),
            Err(_) => output.to_string(),
        }
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
