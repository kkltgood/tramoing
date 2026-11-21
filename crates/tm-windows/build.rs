#[path = "../../packaging/windows/resources.rs"]
mod resources;

fn main() {
    resources::embed("tramoing-helper", "Tramoing 流量采集助手");
}
