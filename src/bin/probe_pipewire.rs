use aetna_volume::backend::{AudioBackend, pipewire_native::PipeWireBackend};

fn main() {
    let backend = PipeWireBackend::new();
    let snapshot = backend.refresh();
    println!(
        "server: {}",
        snapshot.server_name.as_deref().unwrap_or("unknown")
    );
    println!("nodes: {}", snapshot.nodes.len());
    for node in &snapshot.nodes {
        let peer_summary = snapshot
            .peers
            .get(&node.id)
            .map(|peers| {
                let names: Vec<String> = peers
                    .iter()
                    .map(|id| {
                        snapshot
                            .nodes
                            .iter()
                            .find(|n| n.id == *id)
                            .map(|n| format!("#{}", n.id))
                            .unwrap_or_else(|| format!("#{id}?"))
                    })
                    .collect();
                format!(" peers=[{}]", names.join(", "))
            })
            .unwrap_or_default();
        println!(
            "  #{:<4} {:?} {} [{}]{}",
            node.id, node.class, node.description, node.name, peer_summary
        );
    }
    println!("cards: {}", snapshot.cards.len());
    for card in &snapshot.cards {
        println!("  #{:<4} {} [{}]", card.id, card.description, card.name);
    }
    if let Some(error) = snapshot.error {
        eprintln!("error: {error}");
    }
}
