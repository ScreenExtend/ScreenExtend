// FROM https://gist.github.com/wolever/4418079

#import <CoreWLAN/CoreWLAN.h>
#import <objc/message.h>

int main(int argc, char* argv[]) {
  @autoreleasepool {
    int ch;
    NSString *ssid = nil, *password = nil;

    while((ch = getopt(argc, argv, "s:p:h")) != -1) {
      switch(ch) {
      case 's':
        ssid = [NSString stringWithUTF8String:optarg];
        break;
      case 'p':
        password = [NSString stringWithUTF8String:optarg];
        break;
      case '?':
      case 'h':
      default:
        printf("USAGE: %s [-s ssid] [-p password] [-h] command\n", argv[0]);
        printf("\nOPTIONS:\n");
        printf("   -s ssid     SSID\n");
        printf("   -p password WEP password\n");
        printf("   -h          Print help\n");
        printf("\nCOMMAND:\n");
        printf("   status      Print interface mode\n");
        printf("   start       Start Host AP mode\n");
        printf("   stop        Stop Host AP mode\n");
        return 0;
      }
    }

    NSString *command = nil;
    if(argv[optind]) {
      command = [NSString stringWithUTF8String:argv[optind]];
    }

    CWInterface *iface = [[CWWiFiClient sharedWiFiClient] interface];

    if(!command || [command isEqualToString:@"status"]) {
      NSString *mode = nil;
      switch(iface.interfaceMode) {
      case kCWInterfaceModeStation:
        mode = @"Station";
        break;
      case kCWInterfaceModeIBSS:
        mode = @"IBSS";
        break;
      case kCWInterfaceModeHostAP:
        mode = @"HostAP";
        break;
      case kCWInterfaceModeNone:
      default:
        mode = @"None";
      }
      printf("%s\n", [mode UTF8String]);
    } else if([command isEqualToString:@"stop"]) {
      // Stop Host AP mode
      if(getuid() != 0) {
        printf("this may need root (trying anyway)...\n");
      }
        SEL selector = @selector(stopHostAPMode);
        NSMethodSignature *signature = [iface methodSignatureForSelector: selector];
        NSInvocation *invocation =
        [NSInvocation invocationWithMethodSignature:signature];
        invocation.target = iface;
        invocation.selector = selector;

        [invocation invoke];
        printf("Done?");

      //objc_msgSend(iface, @selector(stopHostAPMode));

    } else if([command isEqualToString:@"start"]) {
      if(!ssid) {
        printf("error: an ssid must be specified\n");
        return 1;
      }

      // known security types:
      //   2: no securiry
      //   16: wep
      // Note: values [-127..127] have been tried, and all but these return errors.
      unsigned long long securityType = 2;
      if(password) {
        if([password length] < 10) {
          printf("error: password too short (must be >= 10 characters)\n");
          return 1;
        }
        securityType = 16;
      }

      NSSet *chans = [iface supportedWLANChannels];
      //printf("chan count: %lu\n", [chans count]);

      NSEnumerator *enumerator = [chans objectEnumerator];
      CWChannel *channel;
      while ((channel = [enumerator nextObject])) {
        //printf("channel: %lu\n", [channel channelNumber]);
        if ([channel channelNumber] == 11)
          break;
      }

        printf("Found Channel: %d\n", channel.channelNumber);

        // Start Host AP mode
        NSError *error = nil;
        NSError **errorptr = &error;

        SEL selector = @selector(startHostAPModeWithSSID:securityType:channel:password:error:);
        NSMethodSignature *signature = [iface methodSignatureForSelector: selector];
        NSInvocation *invocation =
        [NSInvocation invocationWithMethodSignature:signature];
        invocation.target = iface;
        invocation.selector = selector;
            NSString * ssidstr = @"Test";
            NSString * pass = @"barbarbarr";
        NSData * ssidArg = [ssidstr dataUsingEncoding:NSUTF8StringEncoding];
        [invocation setArgument: &ssidArg atIndex:2];
        [invocation setArgument: &securityType atIndex:3];
        [invocation setArgument: &channel atIndex:4];
        [invocation setArgument: &pass atIndex:5];
        [invocation setArgument: &errorptr atIndex:6];

        [invocation invoke];
        BOOL success;
        [invocation getReturnValue:&success];

        if (!success) {
            printf("startHostAPModeWithSSID error: %s\n", [(*errorptr).localizedDescription UTF8String]);
            return 1;
        } else {
            printf("Success?\n");
            return 0;
        }
    }

    return 0;
  }
}
