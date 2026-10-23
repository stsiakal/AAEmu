#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <vector>
#include <sstream>
#include "input.h"

namespace input {
#ifdef AADRIVER_INPUT_TEST
// One shared declaration lets the optimized fixture see getter/setter aliasing.
extern int physical;
extern bool modifier_get_supported,modifier_set_supported;
extern void* mouse_slots[29];
extern unsigned char mouse_module[0x5000];
extern bool mouse_locked_flag;
#endif
struct Key { const char* name; int id; int modifier; };
// r208022 keyboard key IDs (keyboard constructor sub_335033xx, ids above VK codes).
// escape = 0, enter = 27, tab = 14, backspace = 13, letters 15..24, number row 1..10,
// f1..f12 = 58..83, arrows/home/end/pgup/pgdn/insert/delete = 96..105.
static const Key keys[] = {{"w",16,0},{"a",29,0},{"s",30,0},{"d",31,0},{"f",32,0},
    {"g",33,0},{"h",34,0},{"j",35,0},{"k",36,0},{"l",37,0},{"space",56,0},
    {"q",15,0},{"e",17,0},{"z",43,0},{"x",44,0},{"c",45,0},{"v",46,0},
    {"b",47,0},{"n",48,0},{"m",49,0},
    // r208022 keyboard constructors 335036BB..33503775: R through P -> IDs18..24.
    {"r",18,0},{"t",19,0},{"y",20,0},{"u",21,0},{"i",22,0},{"o",23,0},{"p",24,0},
    {"1",1,0},{"2",2,0},{"3",3,0},{"4",4,0},{"5",5,0},
    {"6",6,0},{"7",7,0},{"8",8,0},{"9",9,0},{"0",10,0},
    {"escape",0,0},{"enter",27,0},{"tab",14,0},{"backspace",13,0},
    {"f1",58,0},{"f2",59,0},{"f3",60,0},{"f4",61,0},{"f5",62,0},{"f6",63,0},
    {"f7",64,0},{"f8",65,0},{"f9",66,0},{"f10",67,0},{"f11",82,0},{"f12",83,0},
    {"home",96,0},{"up",97,0},{"pgup",98,0},{"left",99,0},{"right",100,0},
    {"end",101,0},{"down",102,0},{"pgdn",103,0},{"insert",104,0},{"delete",105,0}};
// Human spellings for keys whose native symbol name is abbreviated.
struct Alias { const char* name; const char* target; };
static const Alias aliases[] = {{"esc","escape"},{"return","enter"},
    {"pageup","pgup"},{"page_up","pgup"},{"pagedown","pgdn"},{"page_down","pgdn"},
    {"del","delete"},{"ins","insert"}};
// Exact r208022 keyboard symbols (3350381A / 335039B9 / 33503B9E):
// modifier flags include both sides of the native ctrl/shift/alt mask.
static const Key modifiers[] = {{"lctrl",28,0x11},{"shift",41,0x22},{"alt",55,0x44}};
static const char* modifier_names[] = {"ctrl","shift","alt"};
struct Held { const Key* key; int modifiers; ULONGLONG until; };
static std::vector<Held> held;
static bool mouse_held=false;
static bool mouse_pending=false;
static ULONGLONG mouse_until=0;
static int mouse_button=256;
static const char* mouse_button_name() { return mouse_button==257?"mouse2":"mouse1"; }
// r208022 mouse symbols from the CMouse constructor sub_3350465A (RVAs below):
// mouse1..8 = 256..263, mwheel_up/down = 264/265, maxis_x/y = 266/267 (state 8).
static const int wheel_up_id=264,wheel_down_id=265,axis_x_id=266,axis_y_id=267;
static bool wheel_held=false;
static ULONGLONG wheel_until=0;
static int wheel_id=264;
static const char* wheel_name="mwheel_up";
// Native 1.2 UI input is fed by the "ArcheAge" window procedure (x2game FUN_3950a8e0):
// WM_MOUSEMOVE/WM_LBUTTONDOWN/WM_LBUTTONUP/WM_MOUSEWHEEL are translated into the same
// SInputEvents and forwarded to IInput. SendMessageW runs that WndProc on this thread,
// in-process, without touching the desktop cursor or generating OS input.
static int cursor_x=0,cursor_y=0;
static bool mouse_window_mode=false;
#ifndef AADRIVER_INPUT_TEST
static HWND game_window() {
    static HWND cached=NULL;
    if (cached && IsWindow(cached)) return cached;
    struct Find { HWND window; };
    Find find={NULL};
    EnumWindows([](HWND window,LPARAM param) -> BOOL {
        DWORD pid=0;
        GetWindowThreadProcessId(window,&pid);
        if (pid!=GetCurrentProcessId()) return TRUE;
        char name[32]={0};
        if (GetClassNameA(window,name,sizeof(name)) && !strcmp(name,"ArcheAge")) {
            ((Find*)param)->window=window;
            return FALSE;
        }
        return TRUE;
    },(LPARAM)&find);
    cached=find.window;
    return cached;
}
static bool window_input(void) { return game_window()!=NULL; }
static bool window_mouse(UINT message,WPARAM flags,int x,int y) {
    HWND window=game_window();
    return window && SendMessageW(window,message,flags,MAKELPARAM(x,y))==0;
}
#else
static bool window_input(void) { return false; }
static bool window_mouse(UINT,WPARAM,int,int) { return false; }
#endif
// SInputEvent from CryInput.dll sub_3350439C: 32 bytes, value at +24.
struct Event { int device, state; const char* name; int id, unicode, modifiers;
    float value; void* symbol; };
static_assert(sizeof(Event)==32,"x86 SInputEvent");

static void* checked(void* env) {
#ifdef AADRIVER_INPUT_TEST
    return env;
#else
    BYTE* module=(BYTE*)GetModuleHandleA("CryInput.dll");
    if (!module || !env) return NULL;
    IMAGE_NT_HEADERS* nt=(IMAGE_NT_HEADERS*)(module+((IMAGE_DOS_HEADER*)module)->e_lfanew);
    if(nt->FileHeader.TimeDateStamp!=0x543CB7CE || nt->OptionalHeader.SizeOfImage!=0x14000) return NULL;
    void* object=*(void**)((BYTE*)env+16);
    if(!object || (*(void***)object)[8]!=module+0x14D6) return NULL;
    return object;
#endif
}
static int physical_modifiers(void* env) {
#ifdef AADRIVER_INPUT_TEST
    (void)env;
    return modifier_get_supported?physical:0;
#else
    void* object=checked(env); if(!object) return 0;
    BYTE* module=(BYTE*)GetModuleHandleA("CryInput.dll");
    void** vt=*(void***)object;
    // GetModifiers, virtual slot22, reads Input+0x54 (33501E65).
    if(vt[22]!=module+0x1E65) return 0;
    typedef int (__thiscall *Get)(void*);
    return ((Get)vt[22])(object);
#endif
}
static bool modifier_state_supported(void* env) {
#ifdef AADRIVER_INPUT_TEST
    return env && modifier_get_supported && modifier_set_supported;
#else
    void* object=checked(env); if(!object) return false;
    BYTE* module=(BYTE*)GetModuleHandleA("CryInput.dll");
    void** vt=*(void***)object;
    // Exact r208022 vtable at RVA C388: GetModifiers slot22 / SetModifiers slot24.
    return vt[22]==module+0x1E65 && vt[24]==module+0x1E6D;
#endif
}
static void set_modifiers(void* env,int mask) {
#ifdef AADRIVER_INPUT_TEST
    (void)env;
    physical=mask;
#else
    void* object=checked(env);
    typedef void (__thiscall *Set)(void*,int);
    ((Set)(*(void***)object)[24])(object,mask);
#endif
}
struct ModifierShadow {
    void* env; int saved; bool active;
    ModifierShadow(void* e,int mask):env(e),saved(0),active(modifier_state_supported(e)) {
        if(active) { saved=physical_modifiers(env);set_modifiers(env,mask); }
    }
    ~ModifierShadow() { if(active) set_modifiers(env,saved); }
};
#ifndef AADRIVER_INPUT_TEST
struct EnabledInput {
    void* object; bool was_enabled;
    EnabledInput(void* o):object(o),was_enabled(*((BYTE*)o+36)!=0) {
        if(!was_enabled) enable(true);
    }
    ~EnabledInput() { if(!was_enabled) enable(false); }
    void enable(bool value) {
        typedef void (__thiscall *Enable)(void*,bool);
        ((Enable)(*(void***)object)[7])(object,value);
    }
};
#endif
static bool post(void* env,const Key& key,bool down,int mask) {
    Event event={0,down?1:2,key.name,key.id,0,mask,down?1.f:0.f,NULL};
    if(!checked(env)) return false;
    try {
        // Stock UI listeners can query GetModifiers synchronously instead of Event.modifiers.
        // Restore the exact native state before returning; no duration-wide modifier shadow.
        ModifierShadow shadow(env,mask);
#ifdef AADRIVER_INPUT_TEST
        extern bool record(const Event&);
        return record(event);
#else
        void* object=checked(env);
        typedef void (__thiscall *Post)(void*,const Event&);
        void** vt=*(void***)object;
        EnabledInput enabled(object);
        ((Post)vt[8])(object,event);
        return true;
#endif
    } catch(...) { return false; }
}
static int held_modifiers() {
    int mask=0;
    for(size_t i=0;i<held.size();++i) mask|=held[i].modifiers;
    return mask;
}
// Native CMouse::Update (sub_33504DE2) emits wheel and axis events through
// CMessageInput::PostInputEvent (slot 8, RVA 14D6). Dispatch our synthetic events
// through the same slot so listeners see identical event fields.
static bool dispatch(void* env,const Event& event) {
    if(!checked(env)) return false;
    try {
        ModifierShadow shadow(env,event.modifiers);
#ifdef AADRIVER_INPUT_TEST
        extern bool record_mouse(const Event&);
        return record_mouse(event);
#else
        void* object=checked(env);
        EnabledInput enabled(object);
        typedef void (__thiscall *Post)(void*,const Event&);
        ((Post)(*(void***)object)[8])(object,event);
        return true;
#endif
    } catch(...) { return false; }
}
static void release_wheel(void* env) {
    if(!wheel_held) return;
    wheel_held=false;
    // The native Update releases with the current physical modifier state.
    dispatch(env,Event{1,2,wheel_name,wheel_id,0,physical_modifiers(env)|held_modifiers(),0.f,NULL});
}
static bool mouse_slots_supported(void** slots,BYTE* module) {
    // CMessageInput vtable RVA C850, forwarding to the owned mouse device.
    return slots[26]==module+0x4564 && slots[28]==module+0x457C;
}
static bool checked_mouse(void* env) {
    if(!checked(env) || !modifier_state_supported(env)) return false;
#ifdef AADRIVER_INPUT_TEST
    return mouse_slots_supported(mouse_slots,mouse_module);
#else
    void* object=checked(env);
    BYTE* module=(BYTE*)GetModuleHandleA("CryInput.dll");
    if(!mouse_slots_supported(*(void***)object,module)) return false;
    void* device=*(void**)((BYTE*)object+0x68);
    return device && *((int*)((BYTE*)device+4))==1
        && (*(void***)device)[14]==module+0x49E7
        && (*(void***)device)[16]==module+0x4F7C;
#endif
}
static bool mouse_locked(void* env) {
#ifdef AADRIVER_INPUT_TEST
    (void)env;
    return mouse_locked_flag;
#else
    void* object=checked(env);
    typedef bool (__thiscall *Get)(void*);
    return ((Get)(*(void***)object)[28])(object);
#endif
}
static bool mouse_event(void* env,int id,int state,const char* name,float value) {
    if(!checked_mouse(env)) return false;
    Event event={1,state,name,id,0,physical_modifiers(env)|held_modifiers(),value,NULL};
    try {
#ifndef AADRIVER_INPUT_TEST
        void* object=checked(env);
        EnabledInput enabled(object);
#endif
        ModifierShadow shadow(env,event.modifiers);
#ifdef AADRIVER_INPUT_TEST
        extern bool record_mouse(const Event&);
        return record_mouse(event);
#else
        typedef void (__thiscall *Mouse)(void*,const Event&);
        ((Mouse)(*(void***)object)[26])(object,event);
        return true;
#endif
    } catch(...) { return false; }
}
static void mouse_up(void* env) {
    if(mouse_window_mode) {
        window_mouse(mouse_button==257?WM_RBUTTONUP:WM_LBUTTONUP,0,cursor_x,cursor_y);
        return;
    }
    // r208022 sub_335049E7 dispatches both Mouse1 (256) and Mouse2 (257) before
    // the position-lock / SetCursorPos branch, including after a listener locks it.
    if(!mouse_event(env,mouse_button,2,mouse_button_name(),0.f))
        mouse_event(env,mouse_button,2,mouse_button_name(),0.f);
}
static void release_mouse(void* env) {
    if(!mouse_held) return;
    bool pressed=!mouse_pending;
    mouse_held=false;
    mouse_pending=false;
    if(pressed) mouse_up(env);
    mouse_window_mode=false;
}
static void tick_mouse(void* env,ULONGLONG now) {
    if(!mouse_held || now<mouse_until) return;
    if(!mouse_pending) { release_mouse(env);return; }
    if(mouse_window_mode) {
        mouse_pending=false;
        bool right=mouse_button==257;
        if(!window_mouse(WM_MOUSEMOVE,0,cursor_x,cursor_y) ||
           !window_mouse(right?WM_RBUTTONDOWN:WM_LBUTTONDOWN,
                         right?MK_RBUTTON:MK_LBUTTON,cursor_x,cursor_y)) {
            mouse_held=false;
            return;
        }
        mouse_until=now+100;
        return;
    }
    if(!checked_mouse(env) || mouse_locked(env)) {
        // Cancel a queued click without a button-up when no press occurred.
        mouse_held=mouse_pending=false;
        return;
    }
    mouse_pending=false;
    if(!mouse_event(env,mouse_button,1,mouse_button_name(),1.f)) { release_mouse(env);return; }
    mouse_until=now+100;
}
// Hold a wheel press for a few frames; the native Update also presses and releases
// mwheel_up/down on consecutive frames instead of both in one dispatch.
static void tick_wheel(void* env,ULONGLONG now) {
    if(wheel_held && now>=wheel_until) release_wheel(env);
}
static std::string mouse_control(void* env,const std::string& command,int button,ULONGLONG now) {
    bool click=button!=0;
    const char* expected_name=button==257?"mouse_right_click":click?"mouse_click":"mouse_move";
    std::istringstream words(command);
    std::string name,x_text,y_text,extra;
    unsigned coords[2]={0,0};
    if(!(words>>name>>x_text>>y_text) || (words>>extra)
        || name!=expected_name)
        return "2mouse input expects client-pixel x y within 0..4095";
    const std::string* texts[2]={&x_text,&y_text};
    for(int i=0;i<2;++i) for(char digit:*texts[i]) {
        if(digit<'0' || digit>'9' || coords[i]>409 || coords[i]*10+(digit-'0')>4095)
            return "2mouse input expects client-pixel x y within 0..4095";
        coords[i]=coords[i]*10+(digit-'0');
    }
    unsigned x=coords[0],y=coords[1];
    cursor_x=(int)x;cursor_y=(int)y;
    if(click && mouse_held) return "2mouse button already held; release first";
    // The client WndProc translates window messages into the same IInput events and
    // feeds UI hit-testing/hardware-mouse listeners. Prefer it; the CryInput device
    // path below is the fallback when no "ArcheAge" window exists.
    bool window=window_input();
    if(window) {
        if(!window_mouse(WM_MOUSEMOVE,0,cursor_x,cursor_y))
            return "2mouse input unavailable";
        if(!click) { mouse_window_mode=false;return "0mouse moved"; }
    } else {
        if(!checked_mouse(env)) return "2CryInput mouse is not the supported r208022 build";
        // The native locked-position path calls SetCursorPos; never enter that path.
        if(mouse_locked(env)) return "2mouse input unavailable while native mouse is locked";
        if(!mouse_event(env,269,8,"pos_x",(float)x)) return "2mouse input unavailable";
        // A synchronous listener can change mouse-lock state after the first axis.
        if(mouse_locked(env)) return "2mouse input became locked";
        if(!mouse_event(env,270,8,"pos_y",(float)y)) return "2mouse input unavailable";
        if(!click) { mouse_window_mode=false;return "0mouse moved"; }
        if(mouse_locked(env)) return "2mouse input became locked";
    }
    mouse_held=true;
    mouse_pending=true;
    mouse_window_mode=window;
    // Exact native constructor sub_3350465A registers Mouse1=256 and Mouse2=257.
    mouse_button=button;
    // The driver ticks after native Update and before draining new requests. A queued
    // position therefore receives at least one Update before the next eligible press.
    mouse_until=now+50;
    return "0mouse click queued (press after 50 ms, release 100 ms later; verify the resulting action)";
}
static void release_modifiers(void* env,int before,int after) {
    for(int i=2;i>=0;--i) {
        const Key& key=modifiers[i];
        if((before&key.modifier)!=0 && (after&key.modifier)==0) {
            before&=~key.modifier;
            post(env,key,false,physical_modifiers(env)|before);
        }
    }
}
static void drag_cancel(void* env);
static void tick_drag(void* env,ULONGLONG now);
void release(void* env) {
    int mask=held_modifiers();
    // All primary keys leave before any chord modifiers.
    for(size_t i=0;i<held.size();++i) post(env,*held[i].key,false,physical_modifiers(env)|mask);
    drag_cancel(env);
    release_mouse(env);
    release_wheel(env);
    held.clear();
    release_modifiers(env,mask,0);
}
static void tick_at(void* env,ULONGLONG now) {
    // Release before same-deadline keyboard modifiers, with no dependency on the pipe.
    tick_drag(env,now);
    tick_mouse(env,now);
    tick_wheel(env,now);
    for(size_t i=0;i<held.size();) {
        if(now>=held[i].until) {
            int mask=held_modifiers();
            post(env,*held[i].key,false,physical_modifiers(env)|mask);
            held.erase(held.begin()+i);
            // Shared modifiers stay pressed until their last chord ends.
            release_modifiers(env,mask,held_modifiers());
        }
        else ++i;
    }
}
void tick(void* env) { tick_at(env,GetTickCount64()); }
static bool split_modifiers(char* name,int& mask) {
    mask=0;
    char* primary=name;
    while(char* plus=strchr(primary,'+')) {
        *plus=0;
        int part=0;
        for(size_t i=0;i<3;++i) if(!strcmp(primary,modifier_names[i])) part=modifiers[i].modifier;
        if(!part || (mask&part)!=0) return false;
        mask|=part;
        primary=plus+1;
    }
    memmove(name,primary,strlen(primary)+1);
    return true;
}
static const Key* find_key(const char* name) {
    for(size_t i=0;i<sizeof(keys)/sizeof(keys[0]);++i)
        if(!strcmp(name,keys[i].name)) return &keys[i];
    for(size_t i=0;i<sizeof(aliases)/sizeof(aliases[0]);++i)
        if(!strcmp(name,aliases[i].name)) {
            for(size_t j=0;j<sizeof(keys)/sizeof(keys[0]);++j)
                if(!strcmp(aliases[i].target,keys[j].name)) return &keys[j];
        }
    return NULL;
}
static bool parse_signed(const std::string& text,int& value,int limit) {
    if(text.empty()) return false;
    size_t i=0;bool negative=false;
    if(text[0]=='-') { negative=true;i=1; }
    if(i==text.size()) return false;
    int magnitude=0;
    for(;i<text.size();++i) {
        if(text[i]<'0' || text[i]>'9') return false;
        magnitude=magnitude*10+(text[i]-'0');
        if(magnitude>limit) return false;
    }
    value=negative?-magnitude:magnitude;
    return true;
}
static bool is_wheel_command(const std::string& command) {
    size_t end=command.find(' ');
    std::string first=command.substr(0,end);
    size_t plus=first.rfind('+');
    return first.substr(plus==std::string::npos?0:plus+1)=="wheel";
}
static std::string wheel_control(void* env,const std::string& command,ULONGLONG now) {
    std::istringstream words(command);
    std::string name,count_text,extra;
    if(!(words>>name>>count_text) || (words>>extra))
        return "2wheel expects <modifiers+>wheel <notches>";
    if(name.size()>=32) return "2unsupported or duplicate modifier";
    char token[32]={0};
    memcpy(token,name.data(),name.size());
    int mask=0;
    if(!split_modifiers(token,mask)) return "2unsupported or duplicate modifier";
    if(strcmp(token,"wheel")) return "2wheel expects <modifiers+>wheel <notches>";
    int notches=0;
    if(!parse_signed(count_text,notches,50) || notches==0)
        return "2wheel notches must be nonzero with an absolute value of at most 50";
    bool window=window_input();
    if(!window && !checked(env)) return "2CryInput is not the supported r208022 build";
    if(mask!=0 && !modifier_state_supported(env))
        return "2CryInput modifier state is not the supported r208022 build";
    // Complete any previous press before starting a new one.
    release_wheel(env);
    int before=held_modifiers(),down=before;
    for(size_t i=0;i<3;++i) {
        const Key& mod=modifiers[i];
        if((mask&mod.modifier)!=0 && (down&mod.modifier)==0) {
            down|=mod.modifier;
            if(!post(env,mod,true,physical_modifiers(env)|down)) {
                release_modifiers(env,down&~mod.modifier,before);
                return "2wheel input unavailable";
            }
        }
    }
    bool up=notches>0;
    int id=up?wheel_up_id:wheel_down_id;
    const char* symbol=up?"mwheel_up":"mwheel_down";
    if(window) {
        // The WndProc reads the signed high word as the notch delta and posts mwheel_up/down.
        window_mouse(WM_MOUSEWHEEL,MAKEWPARAM(0,(WORD)(notches*WHEEL_DELTA)),cursor_x,cursor_y);
        release_modifiers(env,down,before);
        return "0wheel sent through the client window";
    }
    if(!dispatch(env,Event{1,1,symbol,id,0,physical_modifiers(env)|down,(float)notches,NULL})) {
        release_modifiers(env,down,before);
        return "2wheel input unavailable";
    }
    release_modifiers(env,down,before);
    wheel_held=true;
    wheel_until=now+50;
    wheel_id=id;
    wheel_name=symbol;
    return "0wheel notch press queued (release follows on the next client tick)";
}
static std::string mouse_look_control(void* env,const std::string& command,ULONGLONG now) {
    std::istringstream words(command);
    std::string name,x_text,y_text,extra;
    if(!(words>>name>>x_text>>y_text) || (words>>extra) || name!="mouse_look")
        return "2mouse look expects dx dy within -4095..4095";
    int dx=0,dy=0;
    if(!parse_signed(x_text,dx,4095) || !parse_signed(y_text,dy,4095) || (dx==0 && dy==0))
        return "2mouse look expects dx dy within -4095..4095 (not both zero)";
    if(mouse_held) return "2mouse button already held; release first";
    if(!checked_mouse(env)) return "2CryInput mouse is not the supported r208022 build";
    if(!mouse_event(env,257,1,"mouse2",1.f)) return "2mouse look unavailable";
    int mask=physical_modifiers(env)|held_modifiers();
    int span_x=dx<0?-dx:dx,span_y=dy<0?-dy:dy;
    int span=span_x>span_y?span_x:span_y;
    int steps=(span+99)/100;
    if(steps<1) steps=1;
    int sent_x=0,sent_y=0;
    for(int i=1;i<=steps;++i) {
        int next_x=dx*i/steps,next_y=dy*i/steps;
        int step_x=next_x-sent_x,step_y=next_y-sent_y;
        bool ok=true;
        if(step_x) ok=dispatch(env,Event{1,8,"maxis_x",axis_x_id,0,mask,(float)step_x,NULL});
        if(ok && step_y) ok=dispatch(env,Event{1,8,"maxis_y",axis_y_id,0,mask,(float)step_y,NULL});
        if(!ok) {
            mouse_event(env,257,2,"mouse2",0.f);
            return "2mouse look unavailable";
        }
        sent_x=next_x;sent_y=next_y;
    }
    mouse_held=true;
    mouse_pending=false;
    mouse_button=257;
    mouse_until=now+100;
    return "0mouse look applied (right button released after 100 ms)";
}
// Left-button drag: move to the source, press, cross to the target in several frames, release.
// UI drag-and-drop (for example a skill from the skill book to the action bar) needs the
// button held while the cursor moves, which a single click cannot express.
struct Drag {
    bool active; int phase,step; int x1,y1,x2,y2; ULONGLONG until;
    Drag():active(false),phase(0),step(0),x1(0),y1(0),x2(0),y2(0),until(0) {}
};
static Drag drag;
static bool drag_via_window=false;
#ifdef AADRIVER_INPUT_TEST
static const int drag_steps=4,drag_hold=50,drag_step=40,drag_release_delay=50;
#else
// Slower than a click so the stock UI's drag threshold and hover updates are reached.
static const int drag_steps=8,drag_hold=100,drag_step=60,drag_release_delay=100;
#endif
static bool drag_move(void* env,int x,int y,bool held=false) {
    if(drag_via_window) return window_mouse(WM_MOUSEMOVE,held?MK_LBUTTON:0,x,y);
    return mouse_event(env,269,8,"pos_x",(float)x) && mouse_event(env,270,8,"pos_y",(float)y);
}
static bool drag_press(void* env) {
    if(drag_via_window) return window_mouse(WM_LBUTTONDOWN,MK_LBUTTON,drag.x1,drag.y1);
    return mouse_event(env,256,1,"mouse1",1.f);
}
static bool drag_release(void* env,int x,int y) {
    if(drag_via_window) return window_mouse(WM_LBUTTONUP,0,x,y);
    return mouse_event(env,256,2,"mouse1",0.f);
}
static void drag_cancel(void* env) {
    if(!drag.active) return;
    if(drag.phase>=2) drag_release(env,drag.x2,drag.y2);
    drag.active=false;
}
static std::string drag_control(void* env,const std::string& command,ULONGLONG now) {
    std::istringstream words(command);
    std::string name,x1_text,y1_text,x2_text,y2_text,extra;
    if(!(words>>name>>x1_text>>y1_text>>x2_text>>y2_text) || (words>>extra)
        || name!="mouse_drag")
        return "2mouse drag expects x1 y1 x2 y2 within 0..4095";
    const std::string* texts[4]={&x1_text,&y1_text,&x2_text,&y2_text};
    unsigned coords[4]={0,0,0,0};
    for(int i=0;i<4;++i) for(char digit:*texts[i]) {
        if(digit<'0' || digit>'9' || coords[i]>409 || coords[i]*10+(digit-'0')>4095)
            return "2mouse drag expects x1 y1 x2 y2 within 0..4095";
        coords[i]=coords[i]*10+(digit-'0');
    }
    if(mouse_held || drag.active) return "2mouse button already held; release first";
    if(coords[0]==coords[2] && coords[1]==coords[3])
        return "2mouse drag needs distinct source and target";
    drag_via_window=window_input();
    if(!drag_via_window && !checked_mouse(env))
        return "2CryInput mouse is not the supported r208022 build";
    if(!drag_via_window && mouse_locked(env))
        return "2mouse input unavailable while native mouse is locked";
    drag.active=true;drag.phase=1;drag.step=0;
    drag.x1=(int)coords[0];drag.y1=(int)coords[1];
    drag.x2=(int)coords[2];drag.y2=(int)coords[3];
    cursor_x=drag.x1;cursor_y=drag.y1;
    if(!drag_move(env,drag.x1,drag.y1)) { drag.active=false;return "2mouse drag unavailable"; }
    drag.until=now+drag_hold;
    return "0mouse drag queued (press, moves, release; verify the resulting action)";
}
static void tick_drag(void* env,ULONGLONG now) {
    if(!drag.active || now<drag.until) return;
    if(drag.phase==1) {
        mouse_button=256;mouse_window_mode=drag_via_window;
        if(!drag_press(env)) { drag.active=false;return; }
        drag.phase=2;drag.step=0;drag.until=now+drag_step;
        return;
    }
    if(drag.phase==2) {
        if(drag.step<drag_steps) {
            int step=++drag.step;
            int x=drag.x1+(drag.x2-drag.x1)*step/drag_steps;
            int y=drag.y1+(drag.y2-drag.y1)*step/drag_steps;
            cursor_x=x;cursor_y=y;
            if(!drag_move(env,x,y,true)) { drag_release(env,x,y);drag.active=false;return; }
            drag.until=now+drag_step;
            return;
        }
        drag.phase=3;drag.until=now+drag_release_delay;
        return;
    }
    drag_release(env,drag.x2,drag.y2);
    drag.active=false;
    mouse_window_mode=false;
}
static std::string control_at(void* env,const std::string& command,ULONGLONG now) {
    if(command.compare(0,10,"mouse_drag")==0) return drag_control(env,command,now);
    if(command=="release") { release(env);return "0released"; }
    if(drag.active && (command.compare(0,10,"mouse_move")==0
        || command.compare(0,11,"mouse_click")==0
        || command.compare(0,17,"mouse_right_click")==0
        || command.compare(0,10,"mouse_look")==0))
        return "2mouse drag already queued; release first";
    if(is_wheel_command(command)) return wheel_control(env,command,now);
    if(command.compare(0,10,"mouse_look")==0) return mouse_look_control(env,command,now);
    if(command.compare(0,10,"mouse_move")==0) return mouse_control(env,command,0,now);
    if(command.compare(0,11,"mouse_click")==0) return mouse_control(env,command,256,now);
    if(command.compare(0,17,"mouse_right_click")==0) return mouse_control(env,command,257,now);
    char name[32]={0},extra=0;unsigned duration=0;
    if(sscanf_s(command.c_str(),"%31s %u %c",name,(unsigned)sizeof(name),&duration,&extra,1)!=2
        || duration<1 || duration>10000) return "2input expects a supported key and duration 1..10000 ms";
    int mask=0;
    if(!split_modifiers(name,mask)) return "2unsupported or duplicate modifier";
    const Key* key=find_key(name);
    if(!key) return "2unsupported key";
    if(!checked(env)) return "2CryInput is not the supported r208022 build";
    if((mask|held_modifiers())!=0 && !modifier_state_supported(env))
        return "2CryInput modifier state is not the supported r208022 build";
    for(size_t i=0;i<held.size();++i) if(held[i].key==key) return "2key already held; release first";
    int before=held_modifiers(),down=before;
    for(size_t i=0;i<3;++i) {
        const Key& mod=modifiers[i];
        if((mask&mod.modifier)!=0 && (down&mod.modifier)==0) {
            down|=mod.modifier;
            if(!post(env,mod,true,physical_modifiers(env)|down)) {
                release_modifiers(env,down&~mod.modifier,before);
                return "2input unavailable";
            }
        }
    }
    if(!post(env,*key,true,physical_modifiers(env)|down)) {
        release_modifiers(env,down,before);
        return "2input unavailable";
    }
    held.push_back(Held{key,mask,now+duration});
    return "0pressed (automatic release scheduled)";
}
std::string control(void* env,const std::string& command) { return control_at(env,command,GetTickCount64()); }
}
