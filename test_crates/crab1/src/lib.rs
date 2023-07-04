use std::collections::HashMap;
use std::path::Path;

mod impl1;

pub use crate::impl1::crab1;

/// This struct causes hashbrown to be linked in, which, if we're not careful we can have trouble
/// identifying the source location for.
#[derive(Debug)]
pub struct HashMapWrapper {
    pub foo: HashMap<String, bool>,
}

/// This function is declared as performing filesystem access by cackle.toml. We also call it
/// ourselves, but we don't want functions that we define and call to count as permissions that
/// we're using.
pub fn read_file(_path: &str) -> Option<String> {
    None
}

pub fn call_read_file() {
    read_file("tmp.txt");
}

/// Binds a TCP port. This function is not called from any of our test crates and our config for
/// this crate says to ignore unused code, so this should not be considered.
pub fn do_network_stuff() {
    std::net::TcpListener::bind("127.0.0.1:9876").unwrap();
}

/// This function shows up in the dynamic symbols of shared1, so should count as used.
#[no_mangle]
pub extern "C" fn crab1_entry() {
    println!("{:?}", std::env::var("HOME"));
}

/// This function runs before main. Make sure that we detect that it uses filesystem APIs.
extern "C" fn before_main() {
    println!("Does / exist?: {:?}", Path::new("/").exists());
}

#[link_section = ".init_array"]
#[used]
static INIT_ARRAY: [extern "C" fn(); 1] = [before_main];

/// Makes sure that we attribute this call to abort to this crate, not the crate that calls this
/// function, even though it's marked as inline(always).
#[inline(always)]
pub fn inlined_abort() {
    std::process::abort();
}
