fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let ws = curie_meta::open(std::path::Path::new(&path))?;
    println!("root: {}", ws.root.display());
    for m in &ws.members {
        let (name, version) = match &m.descriptor.kind {
            curie_meta::DescriptorKind::Application(a) => (&a.name, &a.version),
            curie_meta::DescriptorKind::Library(l) => (&l.name, &l.version),
            curie_meta::DescriptorKind::Bom(b) => (&b.name, &b.version),
            curie_meta::DescriptorKind::Workspace(_) => continue,
        };
        println!("  {} {}", name, version);
        for (coord, val) in &m.descriptor.dependencies {
            println!("    dep: {} = {}", coord, val.version());
        }
    }
    Ok(())
}
