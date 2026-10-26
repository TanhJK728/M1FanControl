//! Service intent is rechecked immediately before each normal transport write.
//! A disabled or unreadable marker still permits the exact automatic-release
//! payloads. The wrapped transport retains its confirmed-key/range validation.

use crate::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use crate::trace::Trace;

pub struct IntentSmc<S: WriteSmc, F: Fn() -> Result<bool>> {
    inner: S,
    enabled: F,
}

impl<S: WriteSmc, F: Fn() -> Result<bool>> IntentSmc<S, F> {
    pub fn new(inner: S, enabled: F) -> Self {
        Self { inner, enabled }
    }
}

impl<S: WriteSmc, F: Fn() -> Result<bool>> ReadSmc for IntentSmc<S, F> {
    fn read(&mut self, key: &str) -> Result<Value> {
        self.inner.read(key)
    }

    fn keys(&mut self) -> Result<Vec<String>> {
        self.inner.keys()
    }

    fn trace(&self) -> Trace {
        self.inner.trace()
    }
}

fn automatic_release(key: &str, value: &Value) -> bool {
    let mode_key = key == "Ftst"
        || matches!(key.as_bytes(), [b'F', index, b'M', b'd'] if index.is_ascii_hexdigit());
    mode_key && value.kind == *b"ui8 " && value.bytes.as_slice() == [0]
}

impl<S: WriteSmc, F: Fn() -> Result<bool>> WriteSmc for IntentSmc<S, F> {
    fn write(&mut self, key: &str, value: &Value) -> Result<()> {
        if !automatic_release(key, value) {
            match (self.enabled)() {
                Ok(true) => {}
                Ok(false) => {
                    return Err(Error::Control(format!(
                        "installed service is disabled; refusing write to {key}"
                    )));
                }
                Err(error) => {
                    return Err(Error::Control(format!(
                        "installed service intent unavailable; refusing write to {key}: {error}"
                    )));
                }
            }
        }
        self.inner.write(key, value)
    }
}

/// Read the fixed, root-owned installation marker with the same checks used by
/// ServiceState. This function opens no hardware and changes no service state.
pub fn installed_enabled() -> Result<bool> {
    crate::service_state::installed_enabled()
}
