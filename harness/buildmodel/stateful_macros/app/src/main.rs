#[macros::register(13)]
struct Handle;

#[macros::expose]
impl Handle {
    fn cleanup(&mut self) {}
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn main() {
    println!("{} {}", Handle::value() + 1, helper::value());
}
