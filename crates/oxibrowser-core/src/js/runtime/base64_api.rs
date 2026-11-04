//! Web Base64 works on Latin-1 binary strings, not UTF-8 text.

use super::*;

fn invalid_character(ctx: &mut Context) -> boa_engine::JsError {
    let message = js_string!("The string contains invalid characters");
    if let Ok(value) = ctx.global_object().get(js_string!("DOMException"), ctx)
        && let Some(constructor) = value.as_object()
        && let Ok(error) = constructor.construct(
            &[
                message.clone().into(),
                js_string!("InvalidCharacterError").into(),
            ],
            None,
            ctx,
        )
    {
        return boa_engine::JsError::from_opaque(error.into());
    }
    JsNativeError::typ()
        .with_message(message.to_std_string_lossy())
        .into()
}

pub(super) fn register(ctx: &mut Context) -> JsResult<()> {
    ctx.register_global_callable(
        js_string!("atob"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let input = args
                .first()
                .ok_or_else(|| JsNativeError::typ().with_message("atob requires an argument"))?
                .to_string(ctx)?;
            let input: Vec<u8> = input
                .to_std_string_lossy()
                .bytes()
                .filter(|byte| !matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' '))
                .collect();
            let config = base64::engine::GeneralPurposeConfig::new()
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
                .with_decode_allow_trailing_bits(true);
            let bytes = base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD, config)
                .decode(input)
                .map_err(|_| invalid_character(ctx))?;
            let binary: Vec<u16> = bytes.into_iter().map(u16::from).collect();
            Ok(JsString::from(binary.as_slice()).into())
        }),
    )?;
    ctx.register_global_callable(
        js_string!("btoa"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let input = args
                .first()
                .ok_or_else(|| JsNativeError::typ().with_message("btoa requires an argument"))?
                .to_string(ctx)?;
            let bytes: std::result::Result<Vec<u8>, _> = input.iter().map(u8::try_from).collect();
            let bytes = bytes.map_err(|_| invalid_character(ctx))?;
            Ok(JsString::from(
                base64::engine::general_purpose::STANDARD
                    .encode(bytes)
                    .as_str(),
            )
            .into())
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn web_base64_preserves_all_binary_bytes_and_rejects_invalid_strings() {
        let mut runtime = JsRuntime::new();
        let result = runtime.evaluate(r#"(() => {
            const binary = Array.from({length:256},(_,i)=>String.fromCharCode(i)).join('');
            const errors = [()=>atob('!'),()=>atob('_w=='),()=>atob('a'),()=>btoa('\u0100'),()=>btoa('\ud800')].map(fn=>{try{fn();return 'accepted';}catch(error){return error.name;}});
            return [atob(btoa(binary))===binary,atob(' /w \n')==='\u00ff',btoa('\u00ff')==='/w==',atob('Zh')==='f',errors];
        })()"#).await.unwrap();
        assert!(result.exception.is_none(), "{result:?}");
        assert_eq!(
            result.value,
            Some(serde_json::json!([
                true,
                true,
                true,
                true,
                [
                    "InvalidCharacterError",
                    "InvalidCharacterError",
                    "InvalidCharacterError",
                    "InvalidCharacterError",
                    "InvalidCharacterError"
                ]
            ])),
            "{result:?}"
        );
    }
}
