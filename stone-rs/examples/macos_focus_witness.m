#import <AppKit/AppKit.h>
#import <unistd.h>

#include <stdio.h>

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc != 3) {
            fprintf(stderr, "usage: witness LABEL SIGNAL_DIR\n");
            return 2;
        }
        NSString *label = [NSString stringWithUTF8String:argv[1]];
        NSString *signals = [NSString stringWithUTF8String:argv[2]];
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        NSWindow *window = [[NSWindow alloc] initWithContentRect:NSMakeRect(100, 100, 240, 80)
            styleMask:NSWindowStyleMaskTitled backing:NSBackingStoreBuffered defer:NO];
        [window setTitle:[@"Focus witness " stringByAppendingString:label]];
        [app finishLaunching];
        [window makeKeyAndOrderFront:nil];
        [app activateIgnoringOtherApps:YES];

        NSString *ready = [signals stringByAppendingPathComponent:
            [NSString stringWithFormat:@"%d.ready", getpid()]];
        NSString *stop = [signals stringByAppendingPathComponent:@"stop"];
        NSError *error = nil;
        if (![[NSString stringWithFormat:@"%d\n", getpid()] writeToFile:ready atomically:YES
                encoding:NSUTF8StringEncoding error:&error]) {
            fprintf(stderr, "witness ready write failed: %s\n", error.description.UTF8String);
            return 3;
        }

        NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:25];
        NSFileManager *files = [NSFileManager defaultManager];
        while ([deadline timeIntervalSinceNow] > 0 && ![files fileExistsAtPath:stop]) {
            NSEvent *event = [app nextEventMatchingMask:NSEventMaskAny
                untilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]
                inMode:NSDefaultRunLoopMode dequeue:YES];
            if (event) [app sendEvent:event];
            [app updateWindows];
        }
        [window orderOut:nil];
        return 0;
    }
}
