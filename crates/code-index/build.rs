fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set("FileDescription", "Code Index MCP Server");
        res.set("ProductName", "Code Index MCP");
        res.set("CompanyName", "Regsorm");
        res.set("LegalCopyright", "Copyright (C) 2026 Regsorm");
        res.set("OriginalFilename", "code-index.exe");
        res.set("InternalName", "code-index.exe");
        res.compile().expect("failed to embed Windows resources for code-index");
    }
}
