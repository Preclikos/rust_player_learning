use player::Player;

#[tokio::main]
async fn main() {
    let mut player = Player::new();

    let _ = player
        .open_url("https://preclikos.cz/examples/raw/manifest.mpd")
        .await;

    let _ = player.prepare().await;

    let tracks = player.get_tracks();

    let tracks = tracks.unwrap();
    let selected_video = tracks.video.first().unwrap();
    let selected_representation = selected_video.representations.last().unwrap();

    player.set_video_track(selected_video, selected_representation);

    let test = "aasd";

    player.play().await;
}
