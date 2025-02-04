use player::Player;

fn main() {
    let mut player = Player::new();

    let _ = player.open_url("https://preclikos.cz/examples/raw/manifest.mpd");

    let _ = player.prepare();

    let tracks = player.get_tracks();

    let tracks = tracks.unwrap();
    let selected_video = tracks.video.first().unwrap();
    let selected_representation = selected_video.representations.first().unwrap();

    let test = "aasd";
}
