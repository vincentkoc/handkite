#import <Foundation/Foundation.h>
#import <Vision/Vision.h>
#import <CoreGraphics/CoreGraphics.h>
#include <stdatomic.h>

enum { HK_OCR_FAILED = 0, HK_OCR_SUCCESS = 1, HK_OCR_BUSY = 2, HK_OCR_TIMED_OUT = 3 };

// The caller retains the RGBA buffer until this synchronous function returns.
// Output is bounded JSON owned by malloc; no screenshot or text is logged.
static char *recognize_rgba(const unsigned char *bytes, size_t width, size_t height,
                             const double *rects, size_t count, NSLock *guard, NSMutableDictionary *control) {
    @autoreleasepool {
        @try {
            if (!bytes || width == 0 || height == 0 || width > 4096 || height > 4096 || count == 0 || count > 32) return NULL;
            CGColorSpaceRef space = CGColorSpaceCreateDeviceRGB();
            CGDataProviderRef provider = CGDataProviderCreateWithData(NULL, bytes, width * height * 4, NULL);
            CGImageRef image = CGImageCreate(width, height, 8, 32, width * 4, space,
                                            kCGBitmapByteOrderDefault | kCGImageAlphaLast,
                                            provider, NULL, false, kCGRenderingIntentDefault);
            CGDataProviderRelease(provider);
            CGColorSpaceRelease(space);
            if (!image) return NULL;
            NSMutableArray *rows = [NSMutableArray array];
            BOOL ok = YES;
            for (size_t i = 0; i < count && ok; i++) {
                const double *r = rects + i * 4;
                CGRect crop = CGRectMake(r[0], r[1], r[2], r[3]);
                if (CGRectIsEmpty(crop) || !CGRectContainsRect(CGRectMake(0, 0, width, height), crop)) { ok = NO; break; }
                CGImageRef part = CGImageCreateWithImageInRect(image, crop);
                if (!part) { ok = NO; break; }
                VNRecognizeTextRequest *request = [[VNRecognizeTextRequest alloc] init];
                [guard lock];
                BOOL cancelled = [control[@"cancelled"] boolValue];
                if (!cancelled) control[@"request"] = request;
                [guard unlock];
                if (cancelled) { CGImageRelease(part); ok = NO; break; }
                request.recognitionLevel = VNRequestTextRecognitionLevelAccurate;
                request.recognitionLanguages = @[@"zh-Hans", @"en-US"];
                request.usesLanguageCorrection = NO;
                request.minimumTextHeight = 0.005;
                VNImageRequestHandler *handler = [[VNImageRequestHandler alloc] initWithCGImage:part options:@{}];
                NSError *error = nil;
                ok = [handler performRequests:@[request] error:&error];
                if (ok) for (VNRecognizedTextObservation *observation in request.results) {
                    VNRecognizedText *text = [[observation topCandidates:1] firstObject];
                    if (!text || text.confidence < 0.85 || text.string.length == 0 || text.string.length > 4096) continue;
                    CGRect b = observation.boundingBox;
                    CGRect pixels = CGRectMake(crop.origin.x + b.origin.x * crop.size.width,
                                               crop.origin.y + (1 - b.origin.y - b.size.height) * crop.size.height,
                                               b.size.width * crop.size.width, b.size.height * crop.size.height);
                    // Partial text at an internal crop edge must not become a target.
                    if ((crop.origin.x > 0 && pixels.origin.x < crop.origin.x + 2) ||
                        (crop.origin.y > 0 && pixels.origin.y < crop.origin.y + 2) ||
                        (CGRectGetMaxX(crop) < width && CGRectGetMaxX(pixels) > CGRectGetMaxX(crop) - 2) ||
                        (CGRectGetMaxY(crop) < height && CGRectGetMaxY(pixels) > CGRectGetMaxY(crop) - 2)) continue;
                    [rows addObject:@{@"text": text.string, @"rect": @[@(pixels.origin.x), @(pixels.origin.y), @(pixels.size.width), @(pixels.size.height)]}];
                    if (rows.count > 2048) { ok = NO; break; }
                }
                CGImageRelease(part);
            }
            CGImageRelease(image);
            if (!ok) return NULL;
            NSData *data = [NSJSONSerialization dataWithJSONObject:rows options:0 error:nil];
            if (!data || data.length > 2 * 1024 * 1024) return NULL;
            char *output = malloc(data.length + 1);
            if (!output) return NULL;
            memcpy(output, data.bytes, data.length);
            output[data.length] = 0;
            return output;
        } @catch (NSException *exception) {
            return NULL;
        }
    }
}

// At most one recognition worker can outlive a timed-out caller. Every borrowed
// buffer is copied before dispatch; late output is discarded, never admitted.
char *handkite_recognize_rgba(const unsigned char *bytes, size_t width, size_t height,
                             const double *rects, size_t count, int *status) {
    static atomic_bool busy = false;
    if (!status) return NULL;
    *status = HK_OCR_FAILED;
    bool expected = false;
    if (!bytes || !rects || width == 0 || height == 0 || width > 4096 || height > 4096 || count == 0 || count > 32) return NULL;
    if (!atomic_compare_exchange_strong(&busy, &expected, true)) {
        *status = HK_OCR_BUSY;
        return NULL;
    }
    @autoreleasepool {
        NSData *pixels = [NSData dataWithBytes:bytes length:width * height * 4];
        NSData *regions = [NSData dataWithBytes:rects length:count * 4 * sizeof(double)];
        NSLock *guard = [NSLock new];
        NSMutableDictionary *control = [NSMutableDictionary dictionary];
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        __block char *result = NULL;
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            char *value = recognize_rgba(pixels.bytes, width, height, regions.bytes, count, guard, control);
            [guard lock];
            if ([control[@"cancelled"] boolValue]) free(value); else result = value;
            [guard unlock];
            atomic_store(&busy, false);
            dispatch_semaphore_signal(done);
        });
        long timed_out = dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC));
        [guard lock];
        if (timed_out) {
            control[@"cancelled"] = @YES;
            [(VNRequest *)control[@"request"] cancel];
            free(result); result = NULL;
        }
        char *output = result; result = NULL;
        *status = timed_out ? HK_OCR_TIMED_OUT : (output ? HK_OCR_SUCCESS : HK_OCR_FAILED);
        [guard unlock];
        return output;
    }
}
