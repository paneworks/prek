/* Test-only input driver for the pinned Cage source. No kernel input device or
 * public protocol is created. Only an inherited private socketpair can submit
 * bounded touch events, which flow through Cage's ordinary wlroots handlers. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <wayland-server-core.h>
#include <wlr/backend.h>
#include <wlr/interfaces/wlr_touch.h>
#include "server.h"

struct packet {uint32_t version, operation, time; int32_t id; double x,y;};
_Static_assert(sizeof(struct packet)==32,"touch driver wire size");
struct driver {
    struct cg_server *server;
    struct wlr_touch touch;
    struct wl_event_source *source;
    int fd;
    uint32_t last_time;
    bool contacts[32];
};
static const struct wlr_touch_impl implementation={.name="prek-test-touch"};
static int fail(struct driver *driver) {
    wl_display_terminate(driver->server->wl_display);
    return 0;
}
static int receive_touch(int fd,uint32_t mask,void *data) {
    struct driver *driver=data;
    if(mask&(WL_EVENT_HANGUP|WL_EVENT_ERROR))return fail(driver);
    for(unsigned count=0;count<16;count++) {
        struct packet packet;
        ssize_t size=recv(fd,&packet,sizeof(packet),MSG_DONTWAIT|MSG_TRUNC);
        if(size<0 && (errno==EAGAIN||errno==EWOULDBLOCK))return 0;
        if(size!=(ssize_t)sizeof(packet)||packet.version!=1||packet.time<driver->last_time
            ||packet.id<0||packet.id>=32||!isfinite(packet.x)||!isfinite(packet.y)
            ||packet.x<0||packet.x>1||packet.y<0||packet.y>1)return fail(driver);
        driver->last_time=packet.time;
        switch(packet.operation) {
        case 1: {
            if(driver->contacts[packet.id])return fail(driver);
            driver->contacts[packet.id]=true;
            struct wlr_touch_down_event event={.touch=&driver->touch,.time_msec=packet.time,.touch_id=packet.id,.x=packet.x,.y=packet.y};
            wl_signal_emit_mutable(&driver->touch.events.down,&event);
            break;
        }
        case 2: {
            if(!driver->contacts[packet.id])return fail(driver);
            struct wlr_touch_motion_event event={.touch=&driver->touch,.time_msec=packet.time,.touch_id=packet.id,.x=packet.x,.y=packet.y};
            wl_signal_emit_mutable(&driver->touch.events.motion,&event);
            break;
        }
        case 3: {
            if(!driver->contacts[packet.id])return fail(driver);
            driver->contacts[packet.id]=false;
            struct wlr_touch_up_event event={.touch=&driver->touch,.time_msec=packet.time,.touch_id=packet.id};
            wl_signal_emit_mutable(&driver->touch.events.up,&event);
            break;
        }
        case 4:wl_signal_emit_mutable(&driver->touch.events.frame,&driver->touch);break;
        default:return fail(driver);
        }
        uint32_t receipt=packet.operation;
        if(send(fd,&receipt,sizeof(receipt),MSG_NOSIGNAL)!=(ssize_t)sizeof(receipt))return fail(driver);
    }
    return 0;
}
static int control_fd(void) {
    const char *text=getenv("PREK_TEST_TOUCH_FD");
    const char *backend=getenv("WLR_BACKENDS");
    if(!text||!backend||strcmp(backend,"headless"))return -1;
    char *end;errno=0;long descriptor=strtol(text,&end,10);
    if(errno||*end||descriptor<3||descriptor>1048576)return -1;
    int fd=(int)descriptor,type=0;socklen_t length=sizeof(type);
    if(getsockopt(fd,SOL_SOCKET,SO_TYPE,&type,&length)||type!=SOCK_SEQPACKET)return -1;
    struct sockaddr_un address;length=sizeof(address);
    if(getsockname(fd,(struct sockaddr *)&address,&length)||address.sun_family!=AF_UNIX||length!=sizeof(sa_family_t))return -1;
    struct ucred peer;length=sizeof(peer);
    if(getsockopt(fd,SOL_SOCKET,SO_PEERCRED,&peer,&length)||peer.uid!=geteuid())return -1;
    return fd;
}
bool prek_test_touch_allowed(void) {return control_fd()>=0;}
bool prek_test_touch_start(struct cg_server *server) {
    int fd=control_fd();if(fd<0)return false;
    int flags=fcntl(fd,F_GETFL);
    if(flags<0||fcntl(fd,F_SETFD,FD_CLOEXEC)||fcntl(fd,F_SETFL,flags|O_NONBLOCK))return false;
    /* Cage's primary application must not inherit the driver endpoint. */
    unsetenv("PREK_TEST_TOUCH_FD");
    struct driver *driver=calloc(1,sizeof(*driver));
    if(!driver)return false;
    driver->server=server;driver->fd=fd;
    wlr_touch_init(&driver->touch,&implementation,"Prek owned headless touch");
    driver->touch.width_mm=70;driver->touch.height_mm=140;
    wl_signal_emit_mutable(&server->backend->events.new_input,&driver->touch.base);
    driver->source=wl_event_loop_add_fd(wl_display_get_event_loop(server->wl_display),fd,WL_EVENT_READABLE,receive_touch,driver);
    if(!driver->source){wlr_touch_finish(&driver->touch);free(driver);return false;}
    return true;
}
