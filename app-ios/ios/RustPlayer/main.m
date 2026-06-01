// iOS host bootstrap — EMBEDDED player smoke test.
//
// Unlike the old winit shell (where Rust called UIApplicationMain via winit),
// here the Objective-C host owns the whole app lifecycle and hands the Rust
// player a CAMetalLayer to render into. This mirrors how a real app embeds the
// player as one screen, while staying a self-contained smoke test (the Rust
// side plays the bundled encrypted test stream).
//
// Flow:
//   UIApplicationMain → AppDelegate → PlayerViewController whose view is
//   CAMetalLayer-backed → on first layout, hand the layer to
//   rust_player_start(); on subsequent layouts, rust_player_set_size().

#import <UIKit/UIKit.h>
#import <QuartzCore/QuartzCore.h>
#import <stdint.h>

// Exported by libapp_ios.a (see app-ios/src/lib.rs).
extern void *rust_player_start(void *metal_layer, uint32_t width, uint32_t height);
extern void rust_player_set_size(void *handle, uint32_t width, uint32_t height);
extern void rust_player_destroy(void *handle);

// A UIView whose backing layer is a CAMetalLayer — that's the surface wgpu
// renders into on the Rust side.
@interface MetalView : UIView
@end

@implementation MetalView
+ (Class)layerClass {
    return [CAMetalLayer class];
}
@end

@interface PlayerViewController : UIViewController
@property(nonatomic, assign) void *handle;
@end

@implementation PlayerViewController

- (void)loadView {
    MetalView *view = [[MetalView alloc] initWithFrame:UIScreen.mainScreen.bounds];
    view.backgroundColor = UIColor.blackColor;
    self.view = view;
}

- (void)viewDidLayoutSubviews {
    [super viewDidLayoutSubviews];

    CAMetalLayer *layer = (CAMetalLayer *)self.view.layer;

    // Render at native pixel density.
    CGFloat scale = self.view.window.screen.scale > 0 ? self.view.window.screen.scale
                                                       : UIScreen.mainScreen.scale;
    layer.contentsScale = scale;

    CGSize pts = self.view.bounds.size;
    uint32_t w = (uint32_t)(pts.width * scale);
    uint32_t h = (uint32_t)(pts.height * scale);
    if (w == 0 || h == 0) {
        return;
    }
    layer.drawableSize = CGSizeMake(w, h);

    if (self.handle == NULL) {
        NSLog(@"[host] starting embedded player %ux%u (scale %.1f)", w, h, scale);
        self.handle = rust_player_start((__bridge void *)layer, w, h);
    } else {
        rust_player_set_size(self.handle, w, h);
    }
}

- (void)dealloc {
    if (self.handle != NULL) {
        rust_player_destroy(self.handle);
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
