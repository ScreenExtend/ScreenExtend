// motion-window: real on-screen motion for the macOS real-session e2e test (run-mac.mjs
// --motion window[:drag|noise|scroll]). A borderless window covering one display, animated at
// that display's refresh by a CVDisplayLink, so the host's SCREEN CAPTURE produces the frames.
//
//   motion-window <CGDirectDisplayID> <drag|noise|scroll> <seconds> [--out <file.json>]
//
//   drag    a detailed static background (text-like texture) with a large (~60 %) window-like
//           textured rectangle sweeping around fast: what dragging a window looks like
//   noise   the whole display changes every frame (worst case for the encoder)
//   scroll  a page of text scrolling
//
// Prints one JSON line (and writes it to --out when given): frames presented (layer updates
// committed on the main thread from display-link ticks), display-link ticks, elapsed seconds,
// achieved fps, the display's bounds and nominal refresh.
//
// Build (universal, 10.15 floor, no Xcode):
//   clang -O2 -fobjc-arc -arch arm64 -arch x86_64 -mmacosx-version-min=10.15 \
//     -framework AppKit -framework QuartzCore -framework CoreVideo -framework CoreGraphics \
//     -o motion-window motion-window.m
#import <AppKit/AppKit.h>
#import <CoreVideo/CoreVideo.h>
#import <QuartzCore/QuartzCore.h>
#include <stdatomic.h>
#include <unistd.h>

static NSString *gMode;
static double gSeconds;
static CGDirectDisplayID gDisplay;
static NSString *gOut;
static CALayer *gRoot, *gMover;
static NSMutableArray *gNoise;
static CGSize gSize;           // display size in points
static CGFloat gScale;
static CFTimeInterval gStart;
static long gPresented;
static _Atomic long gTicks;
static _Atomic int gPending;   // a tick is queued for the main thread
static CVDisplayLinkRef gLink;

static CGContextRef bitmap(size_t w, size_t h) {
  CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
  CGContextRef c = CGBitmapContextCreate(NULL, w, h, 8, 0, cs,
                                         kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
  CGColorSpaceRelease(cs);
  return c;
}

// Rows of dark "words" on a light page: the detail a screen of text has.
static void drawText(CGContextRef c, size_t w, size_t h, CGFloat bg, unsigned seed, CGFloat line) {
  srandom(seed);
  CGContextSetRGBFillColor(c, bg, bg, bg, 1);
  CGContextFillRect(c, CGRectMake(0, 0, w, h));
  for (CGFloat y = line * 0.6; y + line < h; y += line) {
    CGFloat x = line;
    while (x < w - line) {
      CGFloat ww = line * (0.4 + (random() % 100) / 25.0);
      if (x + ww > w - line) break;
      CGFloat k = 0.1 + (random() % 30) / 100.0;
      CGContextSetRGBFillColor(c, k, k, k + 0.05 * (random() % 3), 1);
      CGContextFillRect(c, CGRectMake(x, y, ww, line * 0.55));
      x += ww + line * 0.35;
    }
  }
}

static CGImageRef textImage(size_t w, size_t h, CGFloat bg, unsigned seed, CGFloat line) {
  CGContextRef c = bitmap(w, h);
  drawText(c, w, h, bg, seed, line);
  CGImageRef img = CGBitmapContextCreateImage(c);
  CGContextRelease(c);
  return img;
}

static CGImageRef windowImage(size_t w, size_t h) {
  CGContextRef c = bitmap(w, h);
  drawText(c, w, h, 0.97, 7, 22 * gScale);
  CGFloat bar = 28 * gScale;
  CGContextSetRGBFillColor(c, 0.85, 0.86, 0.88, 1);
  CGContextFillRect(c, CGRectMake(0, h - bar, w, bar));
  CGFloat r[3][3] = {{0.99, 0.37, 0.34}, {0.99, 0.74, 0.18}, {0.16, 0.79, 0.25}};
  for (int i = 0; i < 3; i++) {
    CGContextSetRGBFillColor(c, r[i][0], r[i][1], r[i][2], 1);
    CGContextFillEllipseInRect(c, CGRectMake(bar * (0.4 + i * 0.7), h - bar * 0.75, bar * 0.45, bar * 0.45));
  }
  CGContextSetRGBStrokeColor(c, 0.3, 0.3, 0.3, 1);
  CGContextSetLineWidth(c, 2 * gScale);
  CGContextStrokeRect(c, CGRectMake(1, 1, w - 2, h - 2));
  CGImageRef img = CGBitmapContextCreateImage(c);
  CGContextRelease(c);
  return img;
}

static CGImageRef noiseImage(size_t w, size_t h) {
  CGContextRef c = bitmap(w, h);
  uint32_t *p = CGBitmapContextGetData(c);
  size_t stride = CGBitmapContextGetBytesPerRow(c) / 4;
  for (size_t y = 0; y < h; y++) arc4random_buf(p + y * stride, w * 4);
  for (size_t y = 0; y < h; y++)
    for (size_t x = 0; x < w; x++) p[y * stride + x] |= 0xff000000u;  // opaque
  CGImageRef img = CGBitmapContextCreateImage(c);
  CGContextRelease(c);
  return img;
}

static void finish(void) {
  if (gLink) { CVDisplayLinkStop(gLink); }
  double el = CACurrentMediaTime() - gStart;
  CGRect b = CGDisplayBounds(gDisplay);
  CGDisplayModeRef m = CGDisplayCopyDisplayMode(gDisplay);
  double hz = m ? CGDisplayModeGetRefreshRate(m) : 0;
  if (m) CGDisplayModeRelease(m);
  NSString *json = [NSString stringWithFormat:
      @"{\"ok\":%s,\"mode\":\"%@\",\"display\":%u,\"bounds\":[%d,%d,%d,%d],\"scale\":%.1f,"
      @"\"nominal_hz\":%.2f,\"seconds\":%.3f,\"presented\":%ld,\"ticks\":%ld,\"presented_fps\":%.2f}",
      gPresented > 0 ? "true" : "false", gMode, gDisplay, (int)b.origin.x, (int)b.origin.y,
      (int)b.size.width, (int)b.size.height, gScale, hz, el, gPresented, (long)atomic_load(&gTicks),
      el > 0 ? gPresented / el : 0];
  printf("%s\n", json.UTF8String);
  fflush(stdout);
  if (gOut) [json writeToFile:gOut atomically:YES encoding:NSUTF8StringEncoding error:nil];
  exit(gPresented > 0 ? 0 : 3);
}

static void step(void) {
  CFTimeInterval t = CACurrentMediaTime() - gStart;
  [CATransaction begin];
  [CATransaction setDisableActions:YES];
  if ([gMode isEqualToString:@"noise"]) {
    gRoot.contents = gNoise[gPresented % gNoise.count];
  } else if ([gMode isEqualToString:@"scroll"]) {
    CGFloat h = gMover.bounds.size.height, period = h - gSize.height;
    CGFloat off = fmod(gPresented * 6.0, period);  // 6 pt a frame: a brisk scroll
    gMover.position = CGPointMake(gSize.width / 2, h / 2 - off);
  } else {
    CGSize r = gMover.bounds.size;
    CGFloat x = (gSize.width - r.width) * (0.5 + 0.5 * sin(t * 2.3)) + r.width / 2;
    CGFloat y = (gSize.height - r.height) * (0.5 + 0.5 * sin(t * 1.7 + 1.1)) + r.height / 2;
    gMover.position = CGPointMake(x, y);
  }
  [CATransaction commit];
  gPresented++;
}

static CVReturn tick(CVDisplayLinkRef l, const CVTimeStamp *now, const CVTimeStamp *out,
                     CVOptionFlags fi, CVOptionFlags *fo, void *ctx) {
  atomic_fetch_add(&gTicks, 1);
  int expected = 0;
  // One update in flight at a time: a busy main thread drops ticks instead of queueing them.
  if (atomic_compare_exchange_strong(&gPending, &expected, 1)) {
    dispatch_async(dispatch_get_main_queue(), ^{
      atomic_store(&gPending, 0);
      step();
    });
  }
  return kCVReturnSuccess;
}

int main(int argc, const char **argv) {
  @autoreleasepool {
    if (argc < 4) {
      fprintf(stderr, "usage: %s <display-id> <drag|noise|scroll> <seconds> [--out file]\n", argv[0]);
      return 64;
    }
    gDisplay = (CGDirectDisplayID)strtoul(argv[1], NULL, 10);
    gMode = [NSString stringWithUTF8String:argv[2]];
    gSeconds = atof(argv[3]);
    for (int i = 4; i + 1 < argc; i++)
      if (!strcmp(argv[i], "--out")) gOut = [NSString stringWithUTF8String:argv[i + 1]];
    if (![@[ @"drag", @"noise", @"scroll" ] containsObject:gMode] || gSeconds <= 0) return 64;

    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];

    NSScreen *screen = nil;
    for (NSScreen *s in [NSScreen screens]) {
      NSNumber *n = s.deviceDescription[@"NSScreenNumber"];
      if (n && n.unsignedIntValue == gDisplay) screen = s;
    }
    if (!screen) {
      NSString *e = [NSString stringWithFormat:@"{\"ok\":false,\"error\":\"no NSScreen for display %u (%lu screens)\"}",
                                               gDisplay, (unsigned long)[NSScreen screens].count];
      printf("%s\n", e.UTF8String);
      if (gOut) [e writeToFile:gOut atomically:YES encoding:NSUTF8StringEncoding error:nil];
      return 2;
    }
    gSize = screen.frame.size;
    gScale = screen.backingScaleFactor;
    size_t pw = (size_t)(gSize.width * gScale), ph = (size_t)(gSize.height * gScale);

    NSWindow *win = [[NSWindow alloc] initWithContentRect:screen.frame
                                                styleMask:NSWindowStyleMaskBorderless
                                                  backing:NSBackingStoreBuffered
                                                    defer:NO
                                                   screen:screen];
    [win setFrame:screen.frame display:NO];
    win.level = NSStatusWindowLevel;
    win.opaque = YES;
    win.ignoresMouseEvents = YES;
    win.hasShadow = NO;
    win.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces |
                             NSWindowCollectionBehaviorStationary |
                             NSWindowCollectionBehaviorIgnoresCycle;
    NSView *v = win.contentView;
    v.wantsLayer = YES;
    gRoot = v.layer;
    gRoot.contentsScale = gScale;
    gRoot.contentsGravity = kCAGravityResize;

    if ([gMode isEqualToString:@"noise"]) {
      gNoise = [NSMutableArray array];
      for (int i = 0; i < 8; i++) [gNoise addObject:(__bridge_transfer id)noiseImage(pw, ph)];
      gRoot.contents = gNoise[0];
    } else if ([gMode isEqualToString:@"scroll"]) {
      gRoot.backgroundColor = CGColorGetConstantColor(kCGColorWhite);
      gMover = [CALayer layer];
      gMover.contentsScale = gScale;
      gMover.bounds = CGRectMake(0, 0, gSize.width, gSize.height * 4);
      gMover.contents = (__bridge_transfer id)textImage(pw, ph * 4, 0.98, 3, 20 * gScale);
      [gRoot addSublayer:gMover];
    } else {
      gRoot.contents = (__bridge_transfer id)textImage(pw, ph, 0.93, 1, 18 * gScale);
      gMover = [CALayer layer];
      gMover.contentsScale = gScale;
      gMover.bounds = CGRectMake(0, 0, gSize.width * 0.6, gSize.height * 0.6);
      gMover.contents = (__bridge_transfer id)windowImage((size_t)(pw * 0.6), (size_t)(ph * 0.6));
      [gRoot addSublayer:gMover];
    }
    [win orderFrontRegardless];

    // Never outlive the run: if the main thread wedges (its display went away), SIGALRM's default
    // action ends the process anyway.
    alarm((unsigned)gSeconds + 8);
    gStart = CACurrentMediaTime();
    if (CVDisplayLinkCreateWithCGDisplay(gDisplay, &gLink) != kCVReturnSuccess) {
      CVDisplayLinkCreateWithActiveCGDisplays(&gLink);
    }
    CVDisplayLinkSetOutputCallback(gLink, tick, NULL);
    CVDisplayLinkStart(gLink);
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(gSeconds * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{ finish(); });
    [NSApp run];
  }
  return 0;
}
