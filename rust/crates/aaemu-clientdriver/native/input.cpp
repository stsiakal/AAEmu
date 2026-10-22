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
// r208022 keyboard key IDs (escape = 0, number row, tab, Q/W/E..., space = 56).
static const Key keys[] = {{"w",16,0},{"a",29,0},{"s",30,0},{"d",31,0},{"f",32,0},
    {"g",33,0},{"space",56,0},{"q",15,0},{"e",17,0},
    // r208022 keyboard constructors 335036BB..33503775: R through P -> IDs18..24.
    {"r",18,0},{"t",19,0},{"y",20,0},{"u",21,0},{"i",22,0},{"o",23,0},{"p",24,0},
    {"1",1,0},{"2",2,0},{"3",3,0},{"4",4,0},{"5",5,0},
    {"6",6,0},{"7",7,0},{"8",8,0},{"9",9,0},{"0",10,0}};
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
}
static void tick_mouse(void* env,ULONGLONG now) {
    if(!mouse_held || now<mouse_until) return;
    if(!mouse_pending) { release_mouse(env);return; }
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
    if(click && mouse_held) return "2mouse button already held; release first";
    if(!checked_mouse(env)) return "2CryInput mouse is not the supported r208022 build";
    // The native locked-position path calls SetCursorPos; never enter that path.
    if(mouse_locked(env)) return "2mouse input unavailable while native mouse is locked";
    if(!mouse_event(env,269,8,"pos_x",(float)x)) return "2mouse input unavailable";
    // A synchronous listener can change mouse-lock state after the first axis.
    if(mouse_locked(env)) return "2mouse input became locked";
    if(!mouse_event(env,270,8,"pos_y",(float)y)) return "2mouse input unavailable";
    if(!click) return "0mouse moved";
    if(mouse_locked(env)) return "2mouse input became locked";
    mouse_held=true;
    mouse_pending=true;
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
void release(void* env) {
    int mask=held_modifiers();
    // All primary keys leave before any chord modifiers.
    for(size_t i=0;i<held.size();++i) post(env,*held[i].key,false,physical_modifiers(env)|mask);
    release_mouse(env);
    release_wheel(env);
    held.clear();
    release_modifiers(env,mask,0);
}
static void tick_at(void* env,ULONGLONG now) {
    // Release before same-deadline keyboard modifiers, with no dependency on the pipe.
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
    if(!checked(env)) return "2CryInput is not the supported r208022 build";
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
static std::string control_at(void* env,const std::string& command,ULONGLONG now) {
    if(command=="release") { release(env);return "0released"; }
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
    const Key* key=NULL;
    for(size_t i=0;i<sizeof(keys)/sizeof(keys[0]);++i) if(!strcmp(name,keys[i].name)) key=&keys[i];
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
