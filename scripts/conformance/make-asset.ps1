# Generates the conformance beep-flash DASH asset (see examples/conformance.rs).
#
# Output: a CENC-encrypted on-demand DASH set (single file + sidx per rep,
# same layout as production): HEVC 720p+360p video ladder, AAC stereo (en)
# + EAC3 5.1 (cs) audio, 60 s, a full-frame white flash + 1 kHz beep aligned
# at every even second (the Phase-2 HDMI-capture lip-sync measurement points).
#
# Test-only ClearKey (safe to publish):
#   KID 00112233445566778899aabbccddeeff
#   KEY 0123456789abcdef0123456789abcdef
#
# Needs: ffmpeg with an HEVC encoder (hevc_qsv/nvenc/x265) + shaka-packager
# (https://github.com/shaka-project/shaka-packager/releases). ffmpeg's own
# movenc CENC is NOT usable here — its global_sidx+encryption output is
# self-inconsistent ("saio atom found without saiz", senc/trun mismatch).
#
# Encoder notes: closed GOP + every-I-is-IDR + no B-frames, so every sidx
# subsegment starts on a clean IDR (open-GOP CRA starts made the hw decoder
# fail with "Could not find ref with POC …").
#
# HDR10 / HDR10+ / DoVi variants: TODO — planned as additional adaptation
# sets once the SDR set is wired into CI (needs a 10-bit-capable encoder on
# the generating machine and dovi_tool for the RPU).
param(
    [string]$OutDir = "$PSScriptRoot\out",
    [string]$Packager = "packager.exe"
)
$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force $OutDir | Out-Null
Set-Location $OutDir
$KID = "00112233445566778899aabbccddeeff"
$KEY = "0123456789abcdef0123456789abcdef"

$flash = "drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='lt(mod(t\,2)\,0.167)'"
$beep = "volume='if(lt(mod(t\,2)\,0.05),1,0)':eval=frame"

ffmpeg -y -v error -f lavfi -i "testsrc2=duration=60:size=1280x720:rate=24" -vf $flash `
    -c:v hevc_qsv -preset medium -b:v 3000k -g 48 -bf 0 -idr_interval 0 -strict_gop 1 `
    -pix_fmt nv12 -tag:v hvc1 v720.mp4
ffmpeg -y -v error -f lavfi -i "testsrc2=duration=60:size=640x360:rate=24" -vf $flash `
    -c:v hevc_qsv -preset medium -b:v 800k -g 48 -bf 0 -idr_interval 0 -strict_gop 1 `
    -pix_fmt nv12 -tag:v hvc1 v360.mp4
ffmpeg -y -v error -f lavfi -i "sine=frequency=1000:sample_rate=48000:duration=60" `
    -af $beep -c:a aac -b:a 128k -ac 2 aac.mp4
ffmpeg -y -v error -f lavfi -i "sine=frequency=1000:sample_rate=48000:duration=60" `
    -af "$beep,pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" -c:a eac3 -b:a 384k eac3.mp4

& $Packager `
    "in=v360.mp4,stream=video,output=v360-cenc.mp4" `
    "in=v720.mp4,stream=video,output=v720-cenc.mp4" `
    "in=aac.mp4,stream=audio,output=aac-cenc.mp4,lang=en" `
    "in=eac3.mp4,stream=audio,output=eac3-cenc.mp4,lang=cs" `
    --enable_raw_key_encryption --keys "label=:key_id=${KID}:key=${KEY}" `
    --protection_systems CommonSystem --clear_lead 0 --mpd_output manifest.mpd
if ($LASTEXITCODE -ne 0) { throw "packager failed" }

# Round-trip sanity: ffmpeg must decrypt every output cleanly.
foreach ($f in @("v720-cenc.mp4", "v360-cenc.mp4", "aac-cenc.mp4", "eac3-cenc.mp4")) {
    ffmpeg -v error -decryption_key $KEY -i $f -f null NUL
    if ($LASTEXITCODE -ne 0) { throw "round-trip decrypt failed: $f" }
}
Write-Host "Asset ready in $OutDir"
