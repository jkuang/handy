// Isolated AppKit receiver for the ignored Rust timing test. No user clipboard,
// frontmost application, or Accessibility grant is needed.
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <time.h>

@interface FixturePasteboard : NSObject
@property(copy) NSString *text;
@property NSUInteger reads;
@end
@implementation FixturePasteboard
- (NSArray *)types { return @[NSPasteboardTypeString]; }
- (NSArray *)readObjectsForClasses:(NSArray *)classes options:(NSDictionary *)options {
    self.reads++;
    puts("READ");
    fflush(stdout);
    return @[self.text];
}
@end

@interface Receiver : NSTextView
@property uint64_t started, received;
@property NSUInteger downs, ups;
@property BOOL supportsLength;
@property(copy) NSString *atReturn, *previousRead;
@end
@implementation Receiver
- (void)keyDown:(NSEvent *)event {
    NSCAssert(event.keyCode == 36 && !event.modifierFlags, @"expected unmodified Return");
    self.received = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
    self.atReturn = self.string;
    self.downs++;
    [self insertNewline:nil];
}
- (void)keyUp:(NSEvent *)event {
    NSCAssert(event.keyCode == 36, @"expected Return release");
    self.ups++;
}
@end

static id command(NSDictionary *request, Receiver *editor, FixturePasteboard *board) {
    NSString *op = request[@"op"];
    if ([op isEqual:@"begin"]) {
        editor.string = request[@"draft"];
        editor.selectedRange = NSMakeRange([request[@"start"] unsignedIntegerValue],
                                           [request[@"length"] unsignedIntegerValue]);
        board.text = request[@"text"];
        editor.supportsLength = [request[@"supportsLength"] boolValue];
        board.reads = editor.downs = editor.ups = 0;
        editor.atReturn = @"";
        editor.started = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
    } else if ([op isEqual:@"state"]) {
        NSNumber *length = [editor accessibilityAttributeValue:@"AXNumberOfCharacters"];
        NSValue *range = [editor accessibilityAttributeValue:@"AXSelectedTextRange"];
        if (!editor.supportsLength || !length || !range) return NSNull.null;
        return @[length, @(range.rangeValue.location), @(range.rangeValue.length)];
    } else if ([op isEqual:@"slice"]) {
        NSRange range = NSMakeRange([request[@"start"] unsignedIntegerValue],
                                    [request[@"length"] unsignedIntegerValue]);
        return [editor accessibilityAttributeValue:@"AXStringForRange"
                                     forParameter:[NSValue valueWithRange:range]] ?: NSNull.null;
    } else if ([op isEqual:@"read"]) {
        editor.previousRead = [board readObjectsForClasses:@[NSString.class] options:@{}].firstObject;
    } else if ([op isEqual:@"apply_read"]) {
        [editor insertText:editor.previousRead replacementRange:editor.selectedRange];
    } else if ([op isEqual:@"paste"]) {
        BOOL ok = [editor readSelectionFromPasteboard:(NSPasteboard *)board type:NSPasteboardTypeString];
        NSCAssert(ok, @"AppKit refused the paste");
    } else if ([op isEqual:@"event"]) {
        NSArray *bytes = request[@"bytes"];
        NSMutableData *data = [NSMutableData dataWithLength:bytes.count];
        uint8_t *out = data.mutableBytes;
        for (NSNumber *byte in bytes) *out++ = byte.unsignedCharValue;
        CGEventRef cg = CGEventCreateFromData(NULL, (__bridge CFDataRef)data);
        NSCAssert(cg, @"invalid CGEvent");
        NSEvent *event = [NSEvent eventWithCGEvent:cg];
        if (event.type == NSEventTypeKeyDown) [editor keyDown:event];
        else if (event.type == NSEventTypeKeyUp) [editor keyUp:event];
        else abort();
        CFRelease(cg);
    } else if ([op isEqual:@"result"]) {
        return @{@"downs": @(editor.downs), @"ups": @(editor.ups), @"text": editor.atReturn,
                 @"ms": @((editor.received - editor.started) / 1e6)};
    } else {
        abort();
    }
    return @(board.reads);
}

int main(void) {
    @autoreleasepool {
        Receiver *editor = [[Receiver alloc] initWithFrame:NSMakeRect(0, 0, 640, 200)];
        FixturePasteboard *board = [FixturePasteboard new];
        char *line = NULL;
        size_t capacity = 0;
        ssize_t count;
        while ((count = getline(&line, &capacity, stdin)) > 0) @autoreleasepool {
            NSData *data = [NSData dataWithBytes:line length:(NSUInteger)count];
            NSDictionary *request = [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
            id result = command(request, editor, board);
            NSData *reply = [NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingFragmentsAllowed error:NULL];
            fwrite(reply.bytes, 1, reply.length, stdout);
            putchar('\n');
            fflush(stdout);
        }
        free(line);
    }
}
