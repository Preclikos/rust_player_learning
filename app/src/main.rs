use player::Player;

fn main() {
    let mut player = Player::new();

    let _ = player.open_url("http://ftp.itec.aau.at/datasets/DASHDataset2014/BigBuckBunny/4sec/BigBuckBunny_4s_onDemand_2014_05_09.mpd");

    let _ = player.prepare();
}
