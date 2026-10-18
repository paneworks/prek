use super::*;
use std::{cell::RefCell, rc::Rc};

pub(super) fn register<'gc>(
    ctx: &luna::Context<'gc>,
    api: Table<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Result<(), luna::Error<'gc>> {
    api.set(
        *ctx,
        "validate",
        Callback::from_fn(ctx, move |ctx, _, mut stack| {
            if !host.borrow().configuring {
                return Err(io::Error::other(
                    "configuration validation requires configuration evaluation",
                )
                .into());
            }
            let value: Value = stack.consume(ctx)?;
            let config = values::configuration(ctx, value)?;
            config.validate().map_err(io::Error::other)?;
            stack.replace(
                ctx,
                luna_util::serde::to_value(ctx, &config).map_err(io::Error::other)?,
            );
            Ok(CallbackReturn::Return)
        }),
    )?;
    Ok(())
}
