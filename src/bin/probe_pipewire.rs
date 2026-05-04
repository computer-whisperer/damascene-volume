use aetna_volume::backend::{AudioBackend, pipewire_native::PipeWireBackend};

fn main() {
    let mut backend = PipeWireBackend::new();
    let snapshot = backend.refresh();
    println!(
        "server: {}",
        snapshot.server_name.as_deref().unwrap_or("unknown")
    );
    println!("nodes: {}", snapshot.nodes.len());
    for node in &snapshot.nodes {
        println!(
            "  #{:<4} {:?} {} [{}]",
            node.id, node.class, node.description, node.name
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
