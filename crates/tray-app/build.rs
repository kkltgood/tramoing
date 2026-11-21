#[path = "../../packaging/windows/resources.rs"]
mod resources;

fn main() {
    resources::embed("tramoing", "Tramoing 托盘流量监控");
}
