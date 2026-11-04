//! Web Crypto hashing for verified browser assets.

use super::*;
use boa_engine::object::builtins::{AlignedVec, JsArrayBuffer};
use sha2::{Digest, Sha256, Sha384, Sha512};

pub(super) fn register(ctx: &mut Context, crypto: &JsObject) -> JsResult<()> {
    let digest = NativeFunction::from_fn_ptr(|_, args, ctx| {
        let result = (|| {
            let algorithm = args.first().cloned().unwrap_or_default();
            let algorithm = if let Some(object) = algorithm.as_object() {
                object.get(js_string!("name"), ctx)?
            } else {
                algorithm
            };
            let name = algorithm
                .to_string(ctx)?
                .to_std_string_escaped()
                .to_ascii_uppercase();
            if !matches!(name.as_str(), "SHA-256" | "SHA-384" | "SHA-512") {
                let error = boa_engine::JsError::from_native(
                    JsNativeError::error().with_message("Unsupported digest algorithm"),
                )
                .into_opaque(ctx)?;
                if let Some(object) = error.as_object() {
                    object.set(
                        js_string!("name"),
                        js_string!("NotSupportedError"),
                        true,
                        ctx,
                    )?;
                }
                return Err(boa_engine::JsError::from_opaque(error));
            }
            let bytes = copy_buffer_source(args.get(1).unwrap_or(&JsValue::undefined()), ctx)?;
            let hash = match name.as_str() {
                "SHA-256" => Sha256::digest(&bytes).to_vec(),
                "SHA-384" => Sha384::digest(&bytes).to_vec(),
                "SHA-512" => Sha512::digest(&bytes).to_vec(),
                _ => unreachable!(),
            };
            JsArrayBuffer::from_byte_block(AlignedVec::from_iter(0, hash), ctx)
        })();
        match result {
            Ok(buffer) => JsPromise::resolve(buffer, ctx).map(Into::into),
            Err(error) => JsPromise::reject(error, ctx).map(Into::into),
        }
    })
    .to_js_function(ctx.realm());
    let subtle = JsObject::with_object_proto(ctx.intrinsics());
    subtle.create_data_property_or_throw(js_string!("digest"), digest, ctx)?;
    crypto.create_data_property_or_throw(js_string!("subtle"), subtle, ctx)?;
    Ok(())
}
