// iOS host bootstrap — EMBEDDED player with transport controls.
//
// The Objective-C host owns the app lifecycle and a CAMetalLayer-backed view,
// then drives the unified bridge core (`app_shared::bridge`, FFI in
// app-ios/src/lib.rs) through `rustplayer_player_*`. It implements the three host
// callbacks the bridge needs:
//   - event_cb        — unified JSON player events → update the UI
//   - intercept_cb    — provider hook: passthrough (test stream needs no auth)
//   - resolve_key_cb  — provider hook: return the baked ClearKey for a KID
// Both provider callbacks complete the async token synchronously — the bridge
// awaits a oneshot on the Rust side and resumes when we call rustplayer_*_complete.

#import <UIKit/UIKit.h>
#import <QuartzCore/QuartzCore.h>
#import <stdint.h>
#import <stdbool.h>
#import <string.h>

// --- FFI surface exported by libapp_ios.a (see app-ios/src/lib.rs) ----------

typedef void (*rustplayer_intercept_cb)(void *user, const char *url, int kind, uint64_t token);
typedef void (*rustplayer_resolve_key_cb)(void *user, const uint8_t *kid, uint64_t token);
typedef void (*rustplayer_event_cb)(void *user, const char *json);

extern void *rustplayer_player_create(void *metal_layer, uint32_t width, uint32_t height,
                              const char *manifest_url, float start_fraction,
                              int32_t audio_passthrough, bool auto_select_subtitle,
                              rustplayer_intercept_cb intercept_cb, rustplayer_resolve_key_cb resolve_key_cb,
                              rustplayer_event_cb event_cb, void *user);

// Bundled encrypted DASH test stream (smoke test only).
#define TEST_MANIFEST_URL "https://preclikos.cz/examples/encrypted/manifest.mpd"
extern void rustplayer_player_set_size(void *handle, uint32_t width, uint32_t height, float scale);
extern void rustplayer_player_play(void *handle);
extern void rustplayer_player_pause(void *handle);
extern bool rustplayer_player_is_paused(void *handle);
extern void rustplayer_player_seek_ms(void *handle, int64_t position_ms);
extern int64_t rustplayer_player_position_ms(void *handle);
extern int64_t rustplayer_player_duration_ms(void *handle);
extern void rustplayer_player_set_volume(void *handle, float volume);
extern char *rustplayer_player_tracks_json(void *handle);
extern void rustplayer_string_free(char *s);
extern void rustplayer_player_select_video(void *handle, uint32_t adapt, uint32_t repr, bool soft);
extern void rustplayer_player_select_video_auto(void *handle);
extern void rustplayer_player_select_audio(void *handle, uint32_t adapt, uint32_t repr);
extern void rustplayer_player_select_subtitle(void *handle, uint32_t adapt, uint32_t repr);
extern void rustplayer_player_clear_subtitles(void *handle);
extern void rustplayer_player_destroy(void *handle);

// Generic request-filter result (mirrors RustPlayerPreparedRequest in rustplayer_ffi.h).
typedef struct {
    const char *url;
    const char *const *headers;   // [k0,v0,...,NULL] or NULL
    const char *method;           // "GET"/"POST"/... or NULL
    const uint8_t *body;          // optional; NULL = none
    size_t body_len;
} RustPlayerPreparedRequest;

extern void rustplayer_intercept_complete(uint64_t token, const RustPlayerPreparedRequest *prepared);
extern void rustplayer_intercept_fail(uint64_t token, const char *message);
extern void rustplayer_resolve_key_complete(uint64_t token, const uint8_t *key16);
extern void rustplayer_resolve_key_fail(uint64_t token, const char *message);

// --- Metal-backed view -------------------------------------------------------

@interface MetalView : UIView
@end

@implementation MetalView
+ (Class)layerClass {
    return [CAMetalLayer class];
}
@end

@interface PlayerViewController : UIViewController
@property(nonatomic, assign) void *handle;
@property(nonatomic, strong) MetalView *metalView;
@property(nonatomic, strong) UIButton *playPauseButton;
@property(nonatomic, strong) UISlider *seekSlider;
@property(nonatomic, strong) UILabel *timeLabel;
@property(nonatomic, strong) UIButton *tracksButton;
@property(nonatomic, assign) BOOL userSeeking;
@property(nonatomic, assign) double durationMs;
- (void)handleEvent:(NSString *)json;
@end

// --- host callbacks (called from Rust/Tokio worker threads) ------------------

static void event_cb(void *user, const char *json) {
    if (!user || !json) return;
    PlayerViewController *vc = (__bridge PlayerViewController *)user;
    NSString *s = [NSString stringWithUTF8String:json];
    dispatch_async(dispatch_get_main_queue(), ^{
        [vc handleEvent:s];
    });
}

static void intercept_cb(void *user, const char *url, int kind, uint64_t token) {
    (void)user;
    (void)kind;
    // Test provider: pass the URL through unchanged, no auth headers.
    RustPlayerPreparedRequest prepared = {
        .url = url, .headers = NULL, .method = NULL, .body = NULL, .body_len = 0,
    };
    rustplayer_intercept_complete(token, &prepared);
}

static void resolve_key_cb(void *user, const uint8_t *kid, uint64_t token) {
    (void)user;
    // Test provider: baked ClearKeys for the bundled preclikos stream
    // (mirrors app_shared::test_clearkeys()).
    static const uint8_t kid1[16] = {0x0f, 0xd3, 0x7d, 0xac, 0x41, 0xc0, 0xe9, 0x87,
                                      0xe6, 0x8d, 0x43, 0xb8, 0x01, 0xb1, 0x21, 0x0c};
    static const uint8_t key1[16] = {0xfd, 0x8d, 0x9f, 0x40, 0x8c, 0x2b, 0xd7, 0x02,
                                      0x97, 0x0a, 0xfc, 0xd3, 0xb2, 0x19, 0xe7, 0x91};
    static const uint8_t kid2[16] = {0x51, 0x9a, 0xf8, 0x1a, 0xb2, 0xd2, 0x84, 0xf5,
                                      0x2a, 0xa8, 0x25, 0x7d, 0x96, 0xb5, 0xe4, 0xbd};
    static const uint8_t key2[16] = {0x62, 0x7e, 0xf7, 0x2b, 0x42, 0xd9, 0x87, 0x70,
                                      0xde, 0xc2, 0x0e, 0xca, 0xb4, 0x6c, 0xd1, 0xf4};
    if (kid && memcmp(kid, kid1, 16) == 0) {
        rustplayer_resolve_key_complete(token, key1);
    } else if (kid && memcmp(kid, kid2, 16) == 0) {
        rustplayer_resolve_key_complete(token, key2);
    } else {
        rustplayer_resolve_key_fail(token, "no baked key for KID");
    }
}

@implementation PlayerViewController

- (void)loadView {
    UIView *root = [[UIView alloc] initWithFrame:UIScreen.mainScreen.bounds];
    root.backgroundColor = UIColor.blackColor;
    self.view = root;

    self.metalView = [[MetalView alloc] initWithFrame:root.bounds];
    self.metalView.autoresizingMask = UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight;
    self.metalView.backgroundColor = UIColor.blackColor;
    [root addSubview:self.metalView];

    [self buildControls:root];
}

- (void)buildControls:(UIView *)root {
    UIView *bar = [[UIView alloc] init];
    bar.translatesAutoresizingMaskIntoConstraints = NO;
    bar.backgroundColor = [UIColor colorWithWhite:0.0 alpha:0.6];
    [root addSubview:bar];

    self.playPauseButton = [UIButton buttonWithType:UIButtonTypeSystem];
    [self.playPauseButton setTitle:@"❚❚" forState:UIControlStateNormal];
    [self.playPauseButton addTarget:self action:@selector(togglePlay)
                   forControlEvents:UIControlEventTouchUpInside];
    self.playPauseButton.translatesAutoresizingMaskIntoConstraints = NO;

    self.seekSlider = [[UISlider alloc] init];
    self.seekSlider.minimumValue = 0;
    self.seekSlider.maximumValue = 0;
    self.seekSlider.translatesAutoresizingMaskIntoConstraints = NO;
    [self.seekSlider addTarget:self action:@selector(seekBegan)
              forControlEvents:UIControlEventTouchDown];
    [self.seekSlider addTarget:self action:@selector(seekEnded)
              forControlEvents:UIControlEventTouchUpInside | UIControlEventTouchUpOutside];

    self.timeLabel = [[UILabel alloc] init];
    self.timeLabel.text = @"0:00 / 0:00";
    self.timeLabel.textColor = UIColor.whiteColor;
    self.timeLabel.font = [UIFont monospacedDigitSystemFontOfSize:13 weight:UIFontWeightRegular];
    self.timeLabel.translatesAutoresizingMaskIntoConstraints = NO;

    self.tracksButton = [UIButton buttonWithType:UIButtonTypeSystem];
    [self.tracksButton setTitle:@"Tracks" forState:UIControlStateNormal];
    [self.tracksButton addTarget:self action:@selector(showTracks)
                forControlEvents:UIControlEventTouchUpInside];
    self.tracksButton.translatesAutoresizingMaskIntoConstraints = NO;

    [bar addSubview:self.playPauseButton];
    [bar addSubview:self.seekSlider];
    [bar addSubview:self.timeLabel];
    [bar addSubview:self.tracksButton];

    UILayoutGuide *safe = root.safeAreaLayoutGuide;
    [NSLayoutConstraint activateConstraints:@[
        [bar.leadingAnchor constraintEqualToAnchor:root.leadingAnchor],
        [bar.trailingAnchor constraintEqualToAnchor:root.trailingAnchor],
        [bar.bottomAnchor constraintEqualToAnchor:root.bottomAnchor],
        [bar.heightAnchor constraintEqualToConstant:64],

        [self.playPauseButton.leadingAnchor constraintEqualToAnchor:safe.leadingAnchor constant:12],
        [self.playPauseButton.centerYAnchor constraintEqualToAnchor:bar.topAnchor constant:24],
        [self.playPauseButton.widthAnchor constraintEqualToConstant:44],

        [self.seekSlider.leadingAnchor constraintEqualToAnchor:self.playPauseButton.trailingAnchor constant:8],
        [self.seekSlider.centerYAnchor constraintEqualToAnchor:self.playPauseButton.centerYAnchor],

        [self.timeLabel.leadingAnchor constraintEqualToAnchor:self.seekSlider.trailingAnchor constant:8],
        [self.timeLabel.centerYAnchor constraintEqualToAnchor:self.playPauseButton.centerYAnchor],

        [self.tracksButton.leadingAnchor constraintEqualToAnchor:self.timeLabel.trailingAnchor constant:8],
        [self.tracksButton.trailingAnchor constraintEqualToAnchor:safe.trailingAnchor constant:-12],
        [self.tracksButton.centerYAnchor constraintEqualToAnchor:self.playPauseButton.centerYAnchor],
    ]];
}

- (void)viewDidLayoutSubviews {
    [super viewDidLayoutSubviews];

    CAMetalLayer *layer = (CAMetalLayer *)self.metalView.layer;
    CGFloat scale = self.view.window.screen.scale > 0 ? self.view.window.screen.scale
                                                       : UIScreen.mainScreen.scale;
    layer.contentsScale = scale;

    CGSize pts = self.metalView.bounds.size;
    uint32_t w = (uint32_t)(pts.width * scale);
    uint32_t h = (uint32_t)(pts.height * scale);
    if (w == 0 || h == 0) {
        return;
    }
    layer.drawableSize = CGSizeMake(w, h);

    if (self.handle == NULL) {
        NSLog(@"[host] starting embedded player %ux%u (scale %.1f)", w, h, scale);
        self.handle = rustplayer_player_create((__bridge void *)layer, w, h,
                                       TEST_MANIFEST_URL, -1.0f, -1, true,
                                       intercept_cb, resolve_key_cb, event_cb,
                                       (__bridge void *)self);
    } else {
        rustplayer_player_set_size(self.handle, w, h, (float)scale);
    }
}

- (void)togglePlay {
    if (self.handle == NULL) return;
    if (rustplayer_player_is_paused(self.handle)) {
        rustplayer_player_play(self.handle);
    } else {
        rustplayer_player_pause(self.handle);
    }
}

- (void)seekBegan {
    self.userSeeking = YES;
}

- (void)seekEnded {
    self.userSeeking = NO;
    if (self.handle != NULL) {
        rustplayer_player_seek_ms(self.handle, (int64_t)self.seekSlider.value);
    }
}

- (NSString *)formatMs:(double)ms {
    if (ms <= 0) return @"0:00";
    long totalSec = (long)(ms / 1000.0);
    return [NSString stringWithFormat:@"%ld:%02ld", totalSec / 60, totalSec % 60];
}

- (void)handleEvent:(NSString *)json {
    NSData *data = [json dataUsingEncoding:NSUTF8StringEncoding];
    NSDictionary *o = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    if (![o isKindOfClass:[NSDictionary class]]) return;
    NSString *type = o[@"type"];

    if ([type isEqualToString:@"playing"]) {
        [self.playPauseButton setTitle:@"❚❚" forState:UIControlStateNormal];
    } else if ([type isEqualToString:@"paused"]) {
        [self.playPauseButton setTitle:@"▶" forState:UIControlStateNormal];
    } else if ([type isEqualToString:@"position"]) {
        double pos = [o[@"position_ms"] doubleValue];
        double dur = [o[@"duration_ms"] doubleValue];
        if (dur > 0) {
            self.durationMs = dur;
            self.seekSlider.maximumValue = (float)dur;
        }
        if (!self.userSeeking) {
            self.seekSlider.value = (float)pos;
        }
        self.timeLabel.text = [NSString stringWithFormat:@"%@ / %@",
                                                         [self formatMs:pos], [self formatMs:dur]];
    } else if ([type isEqualToString:@"error"]) {
        NSLog(@"[host] player error: %@ — %@", o[@"kind"], o[@"detail"]);
    }
}

- (void)showTracks {
    if (self.handle == NULL) return;
    char *cjson = rustplayer_player_tracks_json(self.handle);
    if (cjson == NULL) return;
    NSString *jsonStr = [NSString stringWithUTF8String:cjson];
    rustplayer_string_free(cjson);

    NSData *data = [jsonStr dataUsingEncoding:NSUTF8StringEncoding];
    NSDictionary *root = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    if (![root isKindOfClass:[NSDictionary class]]) return;

    UIAlertController *sheet = [UIAlertController alertControllerWithTitle:@"Tracks"
                                                                  message:nil
                                                           preferredStyle:UIAlertControllerStyleActionSheet];

    void *handle = self.handle;
    [sheet addAction:[UIAlertAction actionWithTitle:@"Video: Auto (ABR)"
                                              style:UIAlertActionStyleDefault
                                            handler:^(UIAlertAction *a) {
                                                rustplayer_player_select_video_auto(handle);
                                            }]];

    NSArray *video = root[@"video"];
    for (NSDictionary *t in (([video isKindOfClass:[NSArray class]]) ? video : @[])) {
        uint32_t adapt = (uint32_t)[t[@"adapt"] unsignedIntValue];
        uint32_t repr = (uint32_t)[t[@"repr"] unsignedIntValue];
        NSString *label = [NSString stringWithFormat:@"Video: %@", t[@"label"]];
        [sheet addAction:[UIAlertAction actionWithTitle:label
                                                  style:UIAlertActionStyleDefault
                                                handler:^(UIAlertAction *a) {
                                                    rustplayer_player_select_video(handle, adapt, repr, false);
                                                }]];
    }

    NSArray *audio = root[@"audio"];
    for (NSDictionary *t in (([audio isKindOfClass:[NSArray class]]) ? audio : @[])) {
        uint32_t adapt = (uint32_t)[t[@"adapt"] unsignedIntValue];
        uint32_t repr = (uint32_t)[t[@"repr"] unsignedIntValue];
        NSString *label = [NSString stringWithFormat:@"Audio: %@", t[@"label"]];
        [sheet addAction:[UIAlertAction actionWithTitle:label
                                                  style:UIAlertActionStyleDefault
                                                handler:^(UIAlertAction *a) {
                                                    rustplayer_player_select_audio(handle, adapt, repr);
                                                }]];
    }

    [sheet addAction:[UIAlertAction actionWithTitle:@"Subtitles: Off"
                                              style:UIAlertActionStyleDefault
                                            handler:^(UIAlertAction *a) {
                                                rustplayer_player_clear_subtitles(handle);
                                            }]];

    NSArray *text = root[@"text"];
    for (NSDictionary *t in (([text isKindOfClass:[NSArray class]]) ? text : @[])) {
        uint32_t adapt = (uint32_t)[t[@"adapt"] unsignedIntValue];
        uint32_t repr = (uint32_t)[t[@"repr"] unsignedIntValue];
        NSString *label = [NSString stringWithFormat:@"Subtitle: %@", t[@"label"]];
        [sheet addAction:[UIAlertAction actionWithTitle:label
                                                  style:UIAlertActionStyleDefault
                                                handler:^(UIAlertAction *a) {
                                                    rustplayer_player_select_subtitle(handle, adapt, repr);
                                                }]];
    }

    [sheet addAction:[UIAlertAction actionWithTitle:@"Cancel" style:UIAlertActionStyleCancel handler:nil]];

    // iPad requires a popover anchor for action sheets.
    sheet.popoverPresentationController.sourceView = self.tracksButton;
    sheet.popoverPresentationController.sourceRect = self.tracksButton.bounds;

    [self presentViewController:sheet animated:YES completion:nil];
}

- (void)dealloc {
    if (self.handle != NULL) {
        rustplayer_player_destroy(self.handle);
        self.handle = NULL;
    }
}

@end

@interface AppDelegate : UIResponder <UIApplicationDelegate>
@property(nonatomic, strong) UIWindow *window;
@end

@implementation AppDelegate

- (BOOL)application:(UIApplication *)application
    didFinishLaunchingWithOptions:(NSDictionary *)launchOptions {
    self.window = [[UIWindow alloc] initWithFrame:UIScreen.mainScreen.bounds];
    self.window.rootViewController = [PlayerViewController new];
    [self.window makeKeyAndVisible];
    return YES;
}

@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        return UIApplicationMain(argc, argv, nil, NSStringFromClass([AppDelegate class]));
    }
}
