use cfg_aliases::cfg_aliases;

fn main() {
    cfg_aliases! {
        tls: { any(feature = "native-tls", feature = "rustls") },
    }
}
