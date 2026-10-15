/* Instrumented application receiver for isolated Cage/Wayland input tests.
 * The client owns only Wayland/shm/log fds, never evdev or uinput descriptors.
 */
#define _GNU_SOURCE
#include <cairo/cairo.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>
#include "xdg-shell-client-protocol.h"

#define CONTACTS 32
#define KEYS 768
struct contact { bool active; int32_t id; double x, y; };
struct app {
    struct wl_display *display;
    struct wl_compositor *compositor;
    struct wl_shm *shm;
    struct xdg_wm_base *wm;
    struct wl_surface *surface;
    struct wl_seat *seat;
    struct wl_keyboard *keyboard;
    struct wl_touch *touch;
    struct wl_pointer *pointer;
    struct xkb_context *xkb;
    struct xkb_keymap *keymap;
    struct xkb_state *key_state;
    FILE *log;
    bool running, configured, dirty, touch_pending, keys[KEYS];
    uint32_t depressed, latched, locked, group;
    unsigned buffers;
    int width, height;
    uint64_t sequence, frames;
    double pointer_x, pointer_y;
    char text[1024];
    struct contact contacts[CONTACTS];
};
struct drawing { struct app *app; void *pixels; size_t bytes; struct wl_buffer *buffer; };
static void draw(struct app *app);
static uint64_t clock_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) abort();
    return (uint64_t)now.tv_sec * 1000000000 + (uint64_t)now.tv_nsec;
}
static void quoted(FILE *file, const char *text) {
    fputc('"', file);
    for (const unsigned char *p=(const unsigned char *)text; *p; p++) {
        if (*p=='"' || *p=='\\') fprintf(file,"\\%c",*p);
        else if (*p<32) fprintf(file,"\\u%04x",*p);
        else fputc(*p,file);
    }
    fputc('"',file);
}
static void receipt(struct app *app,const char *event,const char *detail) {
    fprintf(app->log,"{\"version\":1,\"sequence\":%" PRIu64 ",\"at_ns\":%" PRIu64 ",\"event\":",
            ++app->sequence,clock_ns());
    quoted(app->log,event);
    if (detail && *detail) fprintf(app->log,",%s",detail);
    fprintf(app->log,",\"width\":%d,\"height\":%d,\"frames\":%" PRIu64 ",\"text\":",app->width,app->height,app->frames);
    quoted(app->log,app->text);
    fprintf(app->log,",\"modifiers\":{\"depressed\":%u,\"latched\":%u,\"locked\":%u,\"group\":%u},\"keys\":[",
            app->depressed,app->latched,app->locked,app->group);
    bool comma=false;
    for (unsigned i=0;i<KEYS;i++) if(app->keys[i]) {fprintf(app->log,"%s%u",comma?",":"",i);comma=true;}
    fprintf(app->log,"],\"contacts\":[");comma=false;
    for (unsigned i=0;i<CONTACTS;i++) if(app->contacts[i].active) {
        struct contact *contact=&app->contacts[i];
        fprintf(app->log,"%s{\"id\":%d,\"x\":%.6f,\"y\":%.6f}",comma?",":"",contact->id,contact->x,contact->y);comma=true;
    }
    fprintf(app->log,"]}\n");
    if (fflush(app->log)) abort();
    app->dirty=true;
    draw(app);
}
static void released(void *data,struct wl_buffer *buffer) {
    struct drawing *drawing=data;
    struct app *app=drawing->app;
    wl_buffer_destroy(buffer);munmap(drawing->pixels,drawing->bytes);free(drawing);
    app->buffers--;
    draw(app);
}
static const struct wl_buffer_listener buffer_listener={.release=released};
static void label(cairo_t *cr,int y,const char *text) {
    cairo_move_to(cr,18,y);cairo_show_text(cr,text);
}
static void draw(struct app *app) {
    if (!app->configured || !app->dirty || app->touch_pending || app->buffers>=3) return;
    /* Visible desktop windows can exceed 2048 logical pixels. Keep both the
       dimensions and total allocation bounded: at most 64 MiB per buffer. */
    if (app->width<1 || app->height<1 || app->width>8192 || app->height>8192
        || (size_t)app->width*app->height>16*1024*1024) abort();
    size_t bytes=(size_t)app->width*app->height*4;
    int fd=memfd_create("prek-wayland-receiver",MFD_CLOEXEC);
    if(fd<0 || ftruncate(fd,(off_t)bytes)) abort();
    void *pixels=mmap(NULL,bytes,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);
    if(pixels==MAP_FAILED) abort();
    struct wl_shm_pool *pool=wl_shm_create_pool(app->shm,fd,(int32_t)bytes);
    struct wl_buffer *buffer=wl_shm_pool_create_buffer(pool,0,app->width,app->height,app->width*4,WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);close(fd);
    struct drawing *drawing=calloc(1,sizeof(*drawing));if(!drawing)abort();
    *drawing=(struct drawing){app,pixels,bytes,buffer};
    wl_buffer_add_listener(buffer,&buffer_listener,drawing);app->buffers++;
    cairo_surface_t *surface=cairo_image_surface_create_for_data(pixels,CAIRO_FORMAT_ARGB32,app->width,app->height,app->width*4);
    cairo_t *cr=cairo_create(surface);
    cairo_set_source_rgb(cr,.07,.10,.15);cairo_paint(cr);
    cairo_set_source_rgb(cr,.85,.94,1);cairo_select_font_face(cr,"monospace",CAIRO_FONT_SLANT_NORMAL,CAIRO_FONT_WEIGHT_NORMAL);cairo_set_font_size(cr,18);
    label(cr,30,"Prek Wayland input receiver");
    char line[1100];snprintf(line,sizeof(line),"receipts: %" PRIu64 "  touch frames: %" PRIu64,app->sequence,app->frames);label(cr,60,line);
    snprintf(line,sizeof(line),"modifiers: %u / %u / %u  group: %u",app->depressed,app->latched,app->locked,app->group);label(cr,90,line);
    snprintf(line,sizeof(line),"text: %s",app->text);label(cr,120,line);
    size_t used=0;used+=(size_t)snprintf(line,sizeof(line),"pressed keys:");
    for(unsigned i=0;i<KEYS && used+16<sizeof(line);i++)if(app->keys[i])used+=(size_t)snprintf(line+used,sizeof(line)-used," %u",i);
    label(cr,150,line);
    for(unsigned i=0;i<CONTACTS;i++)if(app->contacts[i].active) {
        struct contact *contact=&app->contacts[i];
        cairo_set_source_rgb(cr,.2,.8,.8);cairo_arc(cr,contact->x,contact->y,14,0,6.283185307);cairo_fill(cr);
        snprintf(line,sizeof(line),"%d: %.1f, %.1f",contact->id,contact->x,contact->y);
        cairo_move_to(cr,contact->x+18,contact->y);cairo_show_text(cr,line);
    }
    cairo_set_source_rgb(cr,.95,.7,.2);cairo_rectangle(cr,app->pointer_x-4,app->pointer_y-4,8,8);cairo_fill(cr);
    cairo_destroy(cr);cairo_surface_destroy(surface);
    wl_surface_attach(app->surface,buffer,0,0);wl_surface_damage_buffer(app->surface,0,0,app->width,app->height);wl_surface_commit(app->surface);
    app->dirty=false;
}
static void keymap(void *data,struct wl_keyboard *keyboard,uint32_t format,int fd,uint32_t size) {
    struct app *app=data;
    if(format!=WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1 || !size || size>1024*1024){close(fd);abort();}
    char *text=mmap(NULL,size,PROT_READ,MAP_PRIVATE,fd,0);close(fd);
    if(text==MAP_FAILED || text[size-1]!='\0')abort();
    struct xkb_keymap *map=xkb_keymap_new_from_string(app->xkb,text,XKB_KEYMAP_FORMAT_TEXT_V1,XKB_KEYMAP_COMPILE_NO_FLAGS);
    munmap(text,size);if(!map)abort();
    if(app->key_state)xkb_state_unref(app->key_state);if(app->keymap)xkb_keymap_unref(app->keymap);
    app->keymap=map;app->key_state=xkb_state_new(map);if(!app->key_state)abort();
    receipt(app,"keymap",NULL);
}
static void key_enter(void *data,struct wl_keyboard *keyboard,uint32_t serial,struct wl_surface *surface,struct wl_array *keys) {
    struct app *app=data;memset(app->keys,0,sizeof(app->keys));
    uint32_t *key;wl_array_for_each(key,keys){if(*key>=KEYS)abort();app->keys[*key]=true;}
    receipt(app,"keyboard_enter",NULL);
}
static void key_leave(void *data,struct wl_keyboard *keyboard,uint32_t serial,struct wl_surface *surface) {
    struct app *app=data;memset(app->keys,0,sizeof(app->keys));receipt(app,"keyboard_leave",NULL);
}
static void key(void *data,struct wl_keyboard *keyboard,uint32_t serial,uint32_t time,uint32_t code,uint32_t state) {
    struct app *app=data;if(code>=KEYS || state>1)abort();app->keys[code]=state;
    if(state && app->key_state) {
        char text[64];int length=xkb_state_key_get_utf8(app->key_state,code+8,text,sizeof(text));
        size_t used=strlen(app->text);if(length>0 && length<(int)sizeof(text) && used+(size_t)length<sizeof(app->text))memcpy(app->text+used,text,(size_t)length+1);
    }
    char detail[128];snprintf(detail,sizeof(detail),"\"code\":%u,\"state\":%u,\"time_ms\":%u",code,state,time);receipt(app,"key",detail);
}
static void modifiers(void *data,struct wl_keyboard *keyboard,uint32_t serial,uint32_t depressed,uint32_t latched,uint32_t locked,uint32_t group) {
    struct app *app=data;app->depressed=depressed;app->latched=latched;app->locked=locked;app->group=group;
    if(app->key_state)xkb_state_update_mask(app->key_state,depressed,latched,locked,0,0,group);receipt(app,"modifiers",NULL);
}
static void repeat(void *data,struct wl_keyboard *keyboard,int32_t rate,int32_t delay) {
    char detail[100];snprintf(detail,sizeof(detail),"\"rate\":%d,\"delay_ms\":%d",rate,delay);receipt(data,"repeat_info",detail);
}
static const struct wl_keyboard_listener keyboard_listener={.keymap=keymap,.enter=key_enter,.leave=key_leave,.key=key,.modifiers=modifiers,.repeat_info=repeat};
static struct contact *find_contact(struct app *app,int32_t id,bool create) {
    for(unsigned i=0;i<CONTACTS;i++)if(app->contacts[i].active && app->contacts[i].id==id)return &app->contacts[i];
    if(create)for(unsigned i=0;i<CONTACTS;i++)if(!app->contacts[i].active){app->contacts[i]=(struct contact){.active=true,.id=id};return &app->contacts[i];}
    abort();
}
static void touch_down(void *data,struct wl_touch *touch,uint32_t serial,uint32_t time,struct wl_surface *surface,int32_t id,wl_fixed_t x,wl_fixed_t y) {
    struct app *app=data;app->touch_pending=true;struct contact *contact=find_contact(app,id,true);contact->x=wl_fixed_to_double(x);contact->y=wl_fixed_to_double(y);
    char detail[100];snprintf(detail,sizeof(detail),"\"id\":%d,\"time_ms\":%u",id,time);receipt(app,"touch_down",detail);
}
static void touch_up(void *data,struct wl_touch *touch,uint32_t serial,uint32_t time,int32_t id) {
    struct app *app=data;app->touch_pending=true;find_contact(app,id,false)->active=false;
    char detail[100];snprintf(detail,sizeof(detail),"\"id\":%d,\"time_ms\":%u",id,time);receipt(app,"touch_up",detail);
}
static void touch_motion(void *data,struct wl_touch *touch,uint32_t time,int32_t id,wl_fixed_t x,wl_fixed_t y) {
    struct app *app=data;app->touch_pending=true;struct contact *contact=find_contact(app,id,false);contact->x=wl_fixed_to_double(x);contact->y=wl_fixed_to_double(y);
    char detail[100];snprintf(detail,sizeof(detail),"\"id\":%d,\"time_ms\":%u",id,time);receipt(app,"touch_motion",detail);
}
static void touch_frame(void *data,struct wl_touch *touch) {struct app *app=data;app->touch_pending=false;app->frames++;receipt(app,"touch_frame",NULL);}
static void touch_cancel(void *data,struct wl_touch *touch) {struct app *app=data;app->touch_pending=false;memset(app->contacts,0,sizeof(app->contacts));receipt(app,"touch_cancel",NULL);}
static const struct wl_touch_listener touch_listener={.down=touch_down,.up=touch_up,.motion=touch_motion,.frame=touch_frame,.cancel=touch_cancel};
static void pointer_enter(void *data,struct wl_pointer *pointer,uint32_t serial,struct wl_surface *surface,wl_fixed_t x,wl_fixed_t y) {
    struct app *app=data;app->pointer_x=wl_fixed_to_double(x);app->pointer_y=wl_fixed_to_double(y);wl_pointer_set_cursor(pointer,serial,NULL,0,0);receipt(app,"pointer_enter",NULL);
}
static void pointer_leave(void *data,struct wl_pointer *pointer,uint32_t serial,struct wl_surface *surface) {receipt(data,"pointer_leave",NULL);}
static void pointer_motion(void *data,struct wl_pointer *pointer,uint32_t time,wl_fixed_t x,wl_fixed_t y) {
    struct app *app=data;app->pointer_x=wl_fixed_to_double(x);app->pointer_y=wl_fixed_to_double(y);
    char detail[160];snprintf(detail,sizeof(detail),"\"x\":%.6f,\"y\":%.6f,\"time_ms\":%u",app->pointer_x,app->pointer_y,time);receipt(app,"pointer_motion",detail);
}
static void pointer_button(void *data,struct wl_pointer *pointer,uint32_t serial,uint32_t time,uint32_t button,uint32_t state) {
    char detail[128];snprintf(detail,sizeof(detail),"\"button\":%u,\"state\":%u,\"time_ms\":%u",button,state,time);receipt(data,"pointer_button",detail);
}
static void pointer_axis(void *data,struct wl_pointer *pointer,uint32_t time,uint32_t axis,wl_fixed_t value) {
    char detail[128];snprintf(detail,sizeof(detail),"\"axis\":%u,\"value\":%.6f,\"time_ms\":%u",axis,wl_fixed_to_double(value),time);receipt(data,"pointer_axis",detail);
}
static void pointer_frame(void *data,struct wl_pointer *pointer) {receipt(data,"pointer_frame",NULL);}
static void pointer_source(void *data,struct wl_pointer *pointer,uint32_t source) {}
static void pointer_stop(void *data,struct wl_pointer *pointer,uint32_t time,uint32_t axis) {}
static void pointer_discrete(void *data,struct wl_pointer *pointer,uint32_t axis,int32_t discrete) {}
static const struct wl_pointer_listener pointer_listener={.enter=pointer_enter,.leave=pointer_leave,.motion=pointer_motion,.button=pointer_button,.axis=pointer_axis,.frame=pointer_frame,.axis_source=pointer_source,.axis_stop=pointer_stop,.axis_discrete=pointer_discrete};
static void capabilities(void *data,struct wl_seat *seat,uint32_t mask) {
    struct app *app=data;
    if((mask&WL_SEAT_CAPABILITY_KEYBOARD) && !app->keyboard){app->keyboard=wl_seat_get_keyboard(seat);wl_keyboard_add_listener(app->keyboard,&keyboard_listener,app);}
    if((mask&WL_SEAT_CAPABILITY_TOUCH) && !app->touch){app->touch=wl_seat_get_touch(seat);wl_touch_add_listener(app->touch,&touch_listener,app);}
    if((mask&WL_SEAT_CAPABILITY_POINTER) && !app->pointer){app->pointer=wl_seat_get_pointer(seat);wl_pointer_add_listener(app->pointer,&pointer_listener,app);}
    char detail[100];snprintf(detail,sizeof(detail),"\"capabilities\":%u",mask);receipt(app,"seat",detail);
}
static void seat_name(void *data,struct wl_seat *seat,const char *name) {}
static const struct wl_seat_listener seat_listener={.capabilities=capabilities,.name=seat_name};
static void ping(void *data,struct xdg_wm_base *wm,uint32_t serial) {xdg_wm_base_pong(wm,serial);}
static const struct xdg_wm_base_listener wm_listener={.ping=ping};
static void global(void *data,struct wl_registry *registry,uint32_t name,const char *interface,uint32_t version) {
    struct app *app=data;
    if(!strcmp(interface,"wl_compositor"))app->compositor=wl_registry_bind(registry,name,&wl_compositor_interface,version<4?version:4);
    else if(!strcmp(interface,"wl_shm"))app->shm=wl_registry_bind(registry,name,&wl_shm_interface,1);
    else if(!strcmp(interface,"xdg_wm_base")){app->wm=wl_registry_bind(registry,name,&xdg_wm_base_interface,1);xdg_wm_base_add_listener(app->wm,&wm_listener,app);}
    else if(!strcmp(interface,"wl_seat")){app->seat=wl_registry_bind(registry,name,&wl_seat_interface,version<5?version:5);wl_seat_add_listener(app->seat,&seat_listener,app);}
}
static void removed(void *data,struct wl_registry *registry,uint32_t name) {}
static const struct wl_registry_listener registry_listener={.global=global,.global_remove=removed};
static void configure(void *data,struct xdg_surface *surface,uint32_t serial) {struct app *app=data;xdg_surface_ack_configure(surface,serial);app->configured=true;receipt(app,"configured",NULL);}
static const struct xdg_surface_listener surface_listener={.configure=configure};
static void size(void *data,struct xdg_toplevel *toplevel,int32_t width,int32_t height,struct wl_array *states) {struct app *app=data;if(width>0)app->width=width;if(height>0)app->height=height;}
static void closed(void *data,struct xdg_toplevel *toplevel) {((struct app *)data)->running=false;}
static const struct xdg_toplevel_listener toplevel_listener={.configure=size,.close=closed};
int main(int argc,char **argv) {
    const char *vm=getenv("PREK_CAGE_VM"), *host=getenv("PREK_CAGE_HOST");
    if(argc!=2 || !((vm && !strcmp(vm,"1")) || (host && !strcmp(host,"1")))){fprintf(stderr,"use an explicitly owned Cage test session and supply a receipt path\n");return 2;}
    int fd=open(argv[1],O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC,0600);if(fd<0){perror("receipt file");return 1;}
    struct app app={.running=true,.width=600,.height=800};app.log=fdopen(fd,"w");if(!app.log)abort();
    app.xkb=xkb_context_new(XKB_CONTEXT_NO_FLAGS);app.display=wl_display_connect(NULL);if(!app.xkb || !app.display)abort();
    struct wl_registry *registry=wl_display_get_registry(app.display);wl_registry_add_listener(registry,&registry_listener,&app);if(wl_display_roundtrip(app.display)<0)abort();
    if(!app.compositor || !app.shm || !app.wm || !app.seat)abort();
    app.surface=wl_compositor_create_surface(app.compositor);
    struct xdg_surface *surface=xdg_wm_base_get_xdg_surface(app.wm,app.surface);xdg_surface_add_listener(surface,&surface_listener,&app);
    struct xdg_toplevel *toplevel=xdg_surface_get_toplevel(surface);xdg_toplevel_add_listener(toplevel,&toplevel_listener,&app);xdg_toplevel_set_title(toplevel,"Prek input receiver");xdg_toplevel_set_app_id(toplevel,"prek-cage-receiver");
    wl_surface_commit(app.surface);
    while(app.running && wl_display_dispatch(app.display)>=0){}
    app.configured=false;receipt(&app,"closed",NULL);wl_display_disconnect(app.display);fclose(app.log);return 0;
}
