/// The fork ships source and intentionally has no upstream binary hashes.
/// This prevents the build hook from accepting a 2.1.4 upstream artifact
/// whose optimizer predates the forked Rust implementation.
const Map<String, String> assetHashes = <String, String>{};
