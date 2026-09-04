fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set("FileDescription", "BSL Indexer MCP Server");
        res.set("ProductName", "BSL Indexer");
        res.set("CompanyName", "Regsorm");
        res.set("LegalCopyright", "Copyright (C) 2026 Regsorm");
        res.set("OriginalFilename", "bsl-indexer.exe");
        res.set("InternalName", "bsl-indexer.exe");
        res.compile().expect("failed to embed Windows resources for bsl-indexer");
    }
}
