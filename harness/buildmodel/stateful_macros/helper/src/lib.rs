#[macros::register(7)]
struct Handle;

#[macros::expose]
impl Handle {}

pub fn value() -> u32 {
    Handle::value()
}
