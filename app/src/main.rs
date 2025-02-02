use player::Player;

fn main() {
    let mut player = Player::new();

    let _ = player.open_url("https://preclikos.cz/examples/raw/manifest.mpd");

    let _ = player.prepare();
}
