# NOTE: On macOS a specific version of `llvm-ar` and `clang` need to be set here.
# Otherwise the wasm compilation of `rust-secp256k1` will fail.
if [ "$(uname)" == "Darwin" ]; then
  LLVM_PATH=$(brew --prefix llvm)
  # On macs we need to use the brew versions
  AR="${LLVM_PATH}/bin/llvm-ar" CC="${LLVM_PATH}/bin/clang" cargo build --target wasm32-unknown-unknown --release --package inscription_canister
else
  cargo build --target wasm32-unknown-unknown --release --package inscription_canister
fi