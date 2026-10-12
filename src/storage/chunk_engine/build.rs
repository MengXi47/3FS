//! cargo build script：從 `src/cxx.rs` 的 `#[cxx::bridge]` 產生 C++ 端的 header 與膠水碼。
//!
//! 這裡只呼叫 `cxx_build::bridge` 產生程式碼，不在 cargo 內編譯 C++ (回傳的 `cc::Build`
//! 被丟棄)。產物落在 `target/cxxbridge/chunk_engine/src/cxx.rs.{h,cc}`，由 3FS CMake 的
//! `add_crate(chunk_engine)` (`cmake/AddCrate.cmake`) 把 `.cc` 與 `libchunk_engine.a`
//! 一起編進 C++ 靜態庫。

fn main() {
    let _ = cxx_build::bridge("src/cxx.rs");
    println!("cargo:rerun-if-changed=src/cxx.rs");
}
