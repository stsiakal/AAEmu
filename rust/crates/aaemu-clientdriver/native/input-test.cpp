#define AADRIVER_INPUT_TEST
namespace input {
int physical=0;
bool modifier_get_supported=true,modifier_set_supported=true;
void* mouse_slots[29]={0};
unsigned char mouse_module[0x5000]={0};
bool mouse_locked_flag=false,mouse_button_down=false,lock_after_x=false,lock_after_down=false;
float mouse_x=0,mouse_y=0;
}
#include "input.cpp"
#include <assert.h>
#include <stddef.h>
static std::vector<input::Event> events;
static int attempts=0,fail_at=0,throw_at=0;
namespace input {
bool record(const Event& event) {
    // Model a stock synchronous listener querying the native global getter.
    if(modifier_get_supported && modifier_set_supported)
        assert(physical_modifiers((void*)1)==event.modifiers);
    if(attempts+1==throw_at) { ++attempts;throw 1; }
    if(++attempts==fail_at) return false;
    events.push_back(event);return true;
}
bool record_mouse(const Event& event) {
    assert(event.device==1);
    // The native mouse device stores state before dispatching to listeners.
    if(event.id==269) { assert(!mouse_locked_flag);mouse_x=event.value; }
    else if(event.id==270) { assert(!mouse_locked_flag);mouse_y=event.value; }
    else if(event.id==256 || event.id==257) mouse_button_down=event.state==1;
    else assert(event.id==264 || event.id==265 || event.id==266 || event.id==267);
    bool accepted=record(event);
    if((event.id==269 && lock_after_x) || ((event.id==256 || event.id==257) && event.state==1 && lock_after_down))
        mouse_locked_flag=true;
    return accepted;
}
}
static void event(size_t i,int id,bool down,int mask) {
    const input::Event& actual=events.at(i);
    assert(actual.id==id && actual.state==(down?1:2));
    assert(actual.modifiers==mask && actual.value==(down?1.f:0.f));
    assert(actual.device==0 && actual.unicode==0 && actual.symbol==NULL);
    // The exact x86 event bytes consumed by native listeners, including mods+20.
    input::Event expected={0,down?1:2,actual.name,id,0,mask,down?1.f:0.f,NULL};
    assert(!memcmp(&actual,&expected,32));
}
static void mouse_event(size_t i,int id,int state,float value,int mask=0x200) {
    const input::Event& actual=events.at(i);
    const char* name=id==269?"pos_x":id==270?"pos_y":id==257?"mouse2":id==256?"mouse1":
        id==264?"mwheel_up":id==265?"mwheel_down":id==266?"maxis_x":"maxis_y";
    assert(actual.device==1 && actual.state==state && actual.id==id);
    assert(actual.unicode==0 && actual.symbol==NULL && actual.modifiers==mask);
    assert(actual.value==value && !strcmp(actual.name,name));
    input::Event expected={1,state,actual.name,id,0,mask,value,NULL};
    assert(!memcmp(&actual,&expected,32));
}
int main() {
    static_assert(offsetof(input::Event,modifiers)==20,"r208022 modifiers offset");
    static_assert(offsetof(input::Event,value)==24,"r208022 value offset");
    void* env=(void*)1;
    assert(input::control_at(env,"w 100",500)[0]=='0');
    assert(events.size()==1);event(0,16,true,0);
    assert(input::control_at(env,"w 100",500)[0]=='2');
    assert(input::control_at(env,"a 200",500)[0]=='0');
    input::tick_at(env,599);assert(events.size()==2);
    // Deadline is serviced independently of the requester / pipe connection.
    input::tick_at(env,600);event(2,16,false,0);assert(input::held.size()==1);
    input::tick_at(env,700);event(3,29,false,0);assert(input::held.empty());
    size_t before=events.size();
    for(const char* bad: {"w 0","w 10001","w 1 extra","nosuchkey 10","w","w -1",
        "ctrl+ctrl+1 10","meta+1 10","ctrl+ 10","ctrl+shift+alt+ctrl+1 10",
        "+1 10","1+ctrl 10","release 10","ctrl+release 10"})
        assert(input::control_at(env,bad,700)[0]=='2');
    assert(input::held.empty() && events.size()==before);
    assert(input::control_at(NULL,"s 100",700)[0]=='2');
    assert(input::control_at(env,"s 100",700)[0]=='0');
    assert(input::control_at(env,"space 100",700)[0]=='0');
    input::release(env);assert(input::held.empty());event(events.size()-1,56,false,0);

    events.clear();
    input::physical=0x200; // Existing physical CapsLock is retained in every event.
    assert(input::control_at(env,"ctrl+1 100",1000)[0]=='0');
    assert(input::physical==0x200); // Only dispatch sees the synthetic modifier state.
    event(0,28,true,0x211);event(1,1,true,0x211);
    assert(!strcmp(events[0].name,"lctrl") && !strcmp(events[1].name,"1"));
    assert(input::control_at(env,"shift+ctrl+2 200",1000)[0]=='0');
    assert(input::physical==0x200);
    event(2,41,true,0x233);event(3,2,true,0x233);
    assert(input::control_at(env,"alt+1 10",1000)[0]=='2'); // Primary1 already held.
    input::tick_at(env,1099);assert(events.size()==4);
    input::tick_at(env,1100);event(4,1,false,0x233);assert(events.size()==5);
    // Shared ctrl must stay down after the first chord reaches its deadline.
    input::tick_at(env,1200);
    assert(input::physical==0x200);
    event(5,2,false,0x233);event(6,41,false,0x211);event(7,28,false,0x200);
    assert(input::held.empty());

    events.clear();input::physical=0;
    assert(input::control_at(env,"alt+shift+ctrl+0 10000",2000)[0]=='0');
    event(0,28,true,0x11);event(1,41,true,0x33);event(2,55,true,0x77);event(3,10,true,0x77);
    assert(input::control_at(env,"release",2001)=="0released");
    event(4,10,false,0x77);event(5,55,false,0x33);event(6,41,false,0x11);event(7,28,false,0);
    assert(input::held.empty());

    // An unavailable primary post rolls back only newly posted modifiers.
    events.clear();fail_at=attempts+2;
    assert(input::control_at(env,"ctrl+3 10",3000)[0]=='2');
    assert(input::physical==0);
    assert(input::held.empty() && events.size()==2);
    event(0,28,true,0x11);event(1,28,false,0);fail_at=0;
    // Failure in a second modifier must leave an older chord's ctrl hold intact.
    events.clear();
    assert(input::control_at(env,"ctrl+1 100",3100)[0]=='0');
    fail_at=attempts+2;
    assert(input::control_at(env,"ctrl+shift+alt+2 10",3100)[0]=='2');
    assert(input::held.size()==1 && events.size()==4);
    event(2,41,true,0x33);event(3,41,false,0x11);
    fail_at=0;input::tick_at(env,3200);
    event(4,1,false,0x11);event(5,28,false,0);assert(input::held.empty());
    // A C++ listener exception restores all saved physical flags before rollback.
    events.clear();input::physical=0x244;throw_at=attempts+2;
    assert(input::control_at(env,"ctrl+3 10",3300)[0]=='2');
    assert(input::physical==0x244 && input::held.empty() && events.size()==2);
    event(0,28,true,0x255);event(1,28,false,0x244);throw_at=0;
    // Physical modifiers can change between posts; release restores the new snapshot.
    events.clear();input::physical=0x200;
    assert(input::control_at(env,"ctrl+1 10",3400)[0]=='0');
    input::physical=0x444;
    input::tick_at(env,3410);
    event(2,1,false,0x455);event(3,28,false,0x444);
    assert(input::physical==0x444 && input::held.empty());
    // Unsupported Get/Set slots reject an entire modified chord before any mutation.
    // Plain input keeps the existing compatibility path.
    for(int unsupported=0;unsupported<2;++unsupported) {
        events.clear();input::physical=0x200;
        input::modifier_get_supported=unsupported!=0;
        input::modifier_set_supported=unsupported==0;
        int saved_attempts=attempts;
        assert(input::control_at(env,"ctrl+1 10",3500)[0]=='2');
        assert(events.empty() && attempts==saved_attempts && input::physical==0x200);
        assert(input::control_at(env,"w 10",3500)[0]=='0');
        input::tick_at(env,3510);
        assert(events.size()==2 && input::held.empty() && input::physical==0x200);
    }
    input::modifier_get_supported=input::modifier_set_supported=true;
    input::physical=0;
    // Number row IDs, including0 ->10, and deadline cleanup without a pipe call.
    for(int i=0;i<10;++i) {
        events.clear();char command[16];sprintf_s(command,sizeof(command),"%d 1",i);
        assert(input::control_at(env,command,4000)[0]=='0');
        input::tick_at(env,4001);assert(events.size()==2 && input::held.empty());
        event(0,i?i:10,true,0);event(1,i?i:10,false,0);
    }
    // r208022 navigation/function keys and the human-spelling aliases.
    const struct { const char* command; int id; const char* name; } extra[] = {
        {"escape 1",0,"escape"},{"enter 1",27,"enter"},{"return 1",27,"enter"},
        {"tab 1",14,"tab"},{"backspace 1",13,"backspace"},
        {"k 1",36,"k"},{"m 1",49,"m"},{"z 1",43,"z"},
        {"f1 1",58,"f1"},{"f12 1",83,"f12"},
        {"home 1",96,"home"},{"up 1",97,"up"},{"pgup 1",98,"pgup"},
        {"pageup 1",98,"pgup"},{"left 1",99,"left"},{"right 1",100,"right"},
        {"end 1",101,"end"},{"down 1",102,"down"},{"pgdn 1",103,"pgdn"},
        {"pagedown 1",103,"pgdn"},{"insert 1",104,"insert"},{"delete 1",105,"delete"}};
    input::physical=0;
    for(const auto& key: extra) {
        events.clear();
        assert(input::control_at(env,key.command,4050)[0]=='0');
        input::tick_at(env,4051);
        assert(events.size()==2 && input::held.empty());
        event(0,key.id,true,0);event(1,key.id,false,0);
        assert(!strcmp(events[0].name,key.name));
    }
    // Chords apply to the new keys through the same modifier shadow.
    events.clear();input::physical=0x200;
    assert(input::control_at(env,"ctrl+shift+enter 2",4060)[0]=='0');
    assert(events.size()==3);
    event(0,28,true,0x211);event(1,41,true,0x233);event(2,27,true,0x233);
    input::tick_at(env,4062);
    event(3,27,false,0x233);event(4,41,false,0x211);event(5,28,false,0x200);
    input::physical=0;
    // Native mode-bar hotkeys use letter keys as well as the number row.
    // Each exact R..P ID supports a chord, bounded deadline and scoped global restoration.
    const char* letters="rtyuiop";
    input::physical=0x200;
    for(int i=0;i<7;++i) {
        events.clear();char command[16];sprintf_s(command,sizeof(command),"ctrl+%c 2",letters[i]);
        assert(input::control_at(env,command,4100)[0]=='0');
        assert(events.size()==2 && input::physical==0x200);
        event(0,28,true,0x211);event(1,18+i,true,0x211);
        assert(events[1].name[0]==letters[i] && events[1].name[1]==0);
        input::tick_at(env,4101);assert(events.size()==2);
        input::tick_at(env,4102);
        assert(events.size()==4 && input::held.empty() && input::physical==0x200);
        event(2,18+i,false,0x211);event(3,28,false,0x200);
    }
    input::mouse_slots[26]=input::mouse_module+0x4564;
    input::mouse_slots[28]=input::mouse_module+0x457C;
    // Drag validation and the move/press/four-moves/release state machine (CryInput
    // fallback path when no client window exists).
    events.clear();
    for(const char* bad: {"mouse_drag","mouse_drag 1 2","mouse_drag 1 2 3",
        "mouse_drag 1 2 3 4 extra","mouse_drag -1 2 3 4","mouse_drag 1 2 4096 4",
        "mouse_drag 1 2 3 -1","mouse_drag 1 2 1 2","mouse_drag 1.5 2 3 4"})
        assert(input::control_at(env,bad,4900)[0]=='2');
    assert(events.empty() && input::held.empty());
    assert(input::control_at(env,"mouse_drag 10 20 30 40",4900)[0]=='0');
    assert(events.size()==2);
    mouse_event(0,269,8,10);mouse_event(1,270,8,20);
    input::tick_at(env,4950);assert(events.size()==3);mouse_event(2,256,1,1);
    int drag_x=10,drag_y=20;
    for(int step=1;step<=4;++step) {
        input::tick_at(env,4950+step*40);
        assert(events.size()==size_t(3+step*2));
        drag_x=10+(30-10)*step/4;drag_y=20+(40-20)*step/4;
        mouse_event(3+(step-1)*2,269,8,(float)drag_x);
        mouse_event(4+(step-1)*2,270,8,(float)drag_y);
    }
    input::tick_at(env,5150);assert(events.size()==11);
    input::tick_at(env,5200);assert(events.size()==12);
    mouse_event(11,256,2,0);
    // release cancels an unfinished drag and emits an unmatched-free button-up only if pressed.
    events.clear();
    assert(input::control_at(env,"mouse_drag 10 20 30 40",5200)[0]=='0');
    assert(input::control_at(env,"release",5201)=="0released");
    assert(events.size()==2 && !input::mouse_held);
    events.clear();
    assert(input::control_at(env,"mouse_move 123 87",5000)[0]=='0');
    assert(events.size()==2 && input::mouse_x==123 && input::mouse_y==87);
    assert(input::physical==0x200 && !input::mouse_button_down);
    mouse_event(0,269,8,123);mouse_event(1,270,8,87);
    events.clear();
    assert(input::control_at(env,"mouse_click 4095 0",5000)[0]=='0');
    assert(events.size()==2 && input::physical==0x200 && !input::mouse_button_down && input::mouse_held && input::mouse_pending);
    mouse_event(0,269,8,4095);mouse_event(1,270,8,0);
    assert(input::control_at(env,"mouse_click 1 2",5001)[0]=='2');
    assert(input::control_at(env,"mouse_click -1 2",5001)[0]=='2');
    assert(events.size()==2 && input::mouse_held && !input::mouse_button_down);
    input::tick_at(env,5049);assert(events.size()==2 && input::mouse_pending);
    input::tick_at(env,5050);assert(events.size()==3 && input::mouse_button_down && !input::mouse_pending);
    mouse_event(2,256,1,1);
    assert(input::control_at(env,"mouse_click 1 2",5051)[0]=='2');
    assert(events.size()==3 && input::mouse_held && input::mouse_button_down);
    input::tick_at(env,5149);assert(events.size()==3 && input::mouse_button_down);
    // Only the independent driver tick is needed after the requester disconnects.
    input::tick_at(env,5150);
    assert(events.size()==4 && !input::mouse_button_down && !input::mouse_held);
    mouse_event(3,256,2,0);
    // Cancelling the position phase must not send an unmatched button-up.
    events.clear();
    assert(input::control_at(env,"mouse_click 12 34",5000)[0]=='0');
    assert(input::control_at(env,"release",5020)=="0released");
    assert(events.size()==2 && !input::mouse_held && !input::mouse_pending && !input::mouse_button_down);
    input::tick_at(env,5050);assert(events.size()==2);
    // A lock acquired during the render gap cancels the press without further events.
    events.clear();
    assert(input::control_at(env,"mouse_click 12 34",5000)[0]=='0');
    input::mouse_locked_flag=true;input::tick_at(env,5050);
    assert(events.size()==2 && !input::mouse_held && !input::mouse_pending && !input::mouse_button_down);
    input::mouse_locked_flag=false;
    // Keep the later no-mutation assertions tied to the last requested coordinates.
    assert(input::control_at(env,"mouse_move 4095 0",5150)[0]=='0');
    // Invalid coordinates, a locked cursor, or unsupported slots cause no event/state change.
    events.clear();int saved_attempts=attempts;
    for(const char* bad: {"mouse_move","mouse_move 1","mouse_move 1 2 extra",
        "mouse_move -1 0","mouse_click 4096 0","mouse_click 1 -1",
        "mouse_click 1 4096","mouse_move 1.5 0","mouse_movefoo 1 2",
        "mouse_click 4294967296 0","mouse_move 0000000000000000",
        "mouse_move +1 2","mouse_move 1e2 0"})
        assert(input::control_at(env,bad,5000)[0]=='2');
    assert(events.empty() && attempts==saved_attempts && input::mouse_x==4095 && input::mouse_y==0);
    input::mouse_locked_flag=true;
    assert(input::control_at(env,"mouse_move 1 2",5000)[0]=='2');
    assert(input::control_at(env,"mouse_click 1 2",5000)[0]=='2');
    assert(events.empty() && attempts==saved_attempts && !input::mouse_button_down);
    input::mouse_locked_flag=false;
    for(int slot: {26,28}) {
        void* saved=input::mouse_slots[slot];input::mouse_slots[slot]=NULL;
        assert(input::control_at(env,"mouse_click 1 2",5000)[0]=='2');
        assert(events.empty() && attempts==saved_attempts);
        input::mouse_slots[slot]=saved;
    }
    input::modifier_set_supported=false;
    assert(input::control_at(env,"mouse_click 1 2",5000)[0]=='2');
    assert(events.empty() && attempts==saved_attempts && input::physical==0x200);
    input::modifier_set_supported=true;
    // A mouse listener can lock after the first position event; do not send the second axis.
    input::lock_after_x=true;
    assert(input::control_at(env,"mouse_click 12 34",5000)[0]=='2');
    assert(events.size()==1 && !input::mouse_button_down && input::physical==0x200);
    mouse_event(0,269,8,12);input::lock_after_x=input::mouse_locked_flag=false;
    // Button release bypasses the position lock branch, even if a click listener locks it.
    events.clear();input::lock_after_down=true;
    assert(input::control_at(env,"mouse_click 12 34",5000)[0]=='0');
    assert(events.size()==2 && input::mouse_pending && !input::mouse_button_down);
    input::tick_at(env,5050);
    assert(events.size()==3 && input::mouse_button_down && input::mouse_locked_flag);
    input::tick_at(env,5150);
    assert(events.size()==4 && !input::mouse_button_down && !input::mouse_held);
    mouse_event(3,256,2,0);input::lock_after_down=input::mouse_locked_flag=false;
    // A failed or throwing button-down still reaches button-up; a failed up is retried.
    for(int failure: {0,1,2}) {
        events.clear();
        if(failure==0) fail_at=attempts+3;
        else if(failure==1) throw_at=attempts+3;
        else fail_at=attempts+4;
        assert(input::control_at(env,"mouse_click 12 34",5000)[0]=='0');
        assert(events.size()==2 && input::mouse_pending && !input::mouse_button_down);
        input::tick_at(env,5050);
        if(failure==2) {
            assert(input::mouse_button_down && input::mouse_held);
            input::tick_at(env,5150);
        }
        assert(!input::mouse_button_down && input::physical==0x200);
        assert(!input::mouse_held);
        mouse_event(0,269,8,12);mouse_event(1,270,8,34);
        mouse_event(events.size()-1,256,2,0);
        fail_at=throw_at=0;
    }
    // Mouse dispatch sees a held test modifier without consuming the keyboard hold.
    events.clear();
    assert(input::control_at(env,"ctrl+1 200",5100)[0]=='0');
    assert(input::control_at(env,"mouse_click 12 34",5100)[0]=='0');
    mouse_event(2,269,8,12,0x211);
    assert(input::held.size()==1 && input::physical==0x200 && input::mouse_pending && !input::mouse_button_down);
    input::tick_at(env,5150);mouse_event(4,256,1,1,0x211);
    input::tick_at(env,5250);mouse_event(5,256,2,0,0x211);
    assert(input::held.size()==1 && !input::mouse_held && !input::mouse_button_down);
    input::tick_at(env,5300);assert(input::held.empty() && input::physical==0x200);
    // Explicit release cleans both input types before releasing their shared modifiers.
    events.clear();
    assert(input::control_at(env,"ctrl+1 200",5400)[0]=='0');
    assert(input::control_at(env,"mouse_click 12 34",5400)[0]=='0');
    input::tick_at(env,5450);
    assert(input::control_at(env,"release",5451)=="0released");
    assert(events.size()==8 && input::held.empty() && !input::mouse_held && !input::mouse_button_down);
    event(5,1,false,0x211);mouse_event(6,256,2,0,0x211);event(7,28,false,0x200);
    // Equal deadlines release mouse1 while the keyboard modifier is still available.
    events.clear();
    assert(input::control_at(env,"ctrl+1 150",5500)[0]=='0');
    assert(input::control_at(env,"mouse_click 12 34",5500)[0]=='0');
    input::tick_at(env,5550);input::tick_at(env,5650);
    mouse_event(5,256,2,0,0x211);event(6,1,false,0x211);event(7,28,false,0x200);
    assert(input::held.empty() && !input::mouse_held && !input::mouse_button_down);
    // The hold ends 100 ms after the actual delayed press, not the planned press time.
    events.clear();
    assert(input::control_at(env,"mouse_click 12 34",5800)[0]=='0');
    input::tick_at(env,5875);
    assert(events.size()==3 && input::mouse_button_down && !input::mouse_pending);
    input::tick_at(env,5974);assert(events.size()==3 && input::mouse_button_down);
    input::tick_at(env,5975);
    assert(events.size()==4 && !input::mouse_held && !input::mouse_button_down);
    // Mouse2 follows the same render gap, exact event bytes and independent release.
    // Native r208022 handles both button IDs before its position-lock/cursor branch.
    events.clear();input::lock_after_down=true;
    assert(input::control_at(env,"mouse_right_click 88 99",6000)[0]=='0');
    assert(events.size()==2 && input::mouse_pending && !input::mouse_button_down);
    mouse_event(0,269,8,88);mouse_event(1,270,8,99);
    assert(input::control_at(env,"mouse_click 1 2",6001)[0]=='2');
    assert(input::control_at(env,"mouse_right_click 1 2",6001)[0]=='2');
    assert(events.size()==2 && input::mouse_x==88 && input::mouse_y==99);
    input::tick_at(env,6049);assert(events.size()==2);
    input::tick_at(env,6050);
    assert(events.size()==3 && input::mouse_button_down && input::mouse_locked_flag);
    mouse_event(2,257,1,1);
    input::tick_at(env,6149);assert(events.size()==3);
    input::tick_at(env,6150);
    assert(events.size()==4 && !input::mouse_held && !input::mouse_button_down);
    mouse_event(3,257,2,0);
    input::lock_after_down=input::mouse_locked_flag=false;
    // A locked cursor prevents every position event, before the right-button queue exists.
    events.clear();saved_attempts=attempts;input::mouse_locked_flag=true;
    assert(input::control_at(env,"mouse_right_click 1 2",6200)[0]=='2');
    assert(events.empty() && attempts==saved_attempts && !input::mouse_held);
    assert(input::mouse_x==88 && input::mouse_y==99);
    input::mouse_locked_flag=false;
    // Mid-position lock: stop before pos_y, never queue a button and never reposition desktop.
    input::lock_after_x=true;
    assert(input::control_at(env,"mouse_right_click 12 34",6200)[0]=='2');
    assert(events.size()==1 && !input::mouse_held && !input::mouse_button_down);
    mouse_event(0,269,8,12);
    input::lock_after_x=input::mouse_locked_flag=false;
    // Cancellation or a lock during the render gap emits no unmatched Mouse2 release.
    for(bool locked: {false,true}) {
        events.clear();
        assert(input::control_at(env,"mouse_right_click 12 34",6300)[0]=='0');
        if(locked) { input::mouse_locked_flag=true;input::tick_at(env,6350); }
        else assert(input::control_at(env,"release",6310)=="0released");
        input::tick_at(env,6450);
        assert(events.size()==2 && !input::mouse_held && !input::mouse_pending && !input::mouse_button_down);
        input::mouse_locked_flag=false;
    }
    // Failed/throwing Mouse2 presses release Mouse2; a failed release is retried.
    for(int failure: {0,1,2}) {
        events.clear();
        if(failure==0) fail_at=attempts+3;
        else if(failure==1) throw_at=attempts+3;
        else fail_at=attempts+4;
        assert(input::control_at(env,"mouse_right_click 12 34",6500)[0]=='0');
        input::tick_at(env,6550);
        if(failure==2) input::tick_at(env,6650);
        assert(!input::mouse_held && !input::mouse_button_down && input::physical==0x200);
        mouse_event(events.size()-1,257,2,0);
        fail_at=throw_at=0;
    }
    // Unknown commands and unsupported native slots cannot emit Mouse2 events.
    events.clear();saved_attempts=attempts;
    for(const char* bad: {"mouse_right_click", "mouse_right_click 1", "mouse_right_click 1 2 extra",
        "mouse_right_click -1 0", "mouse_right_click 4096 0", "mouse_right_click 1.5 0",
        "mouse_right_clickfoo 1 2", "mouse_right_click 4294967296 0"})
        assert(input::control_at(env,bad,6700)[0]=='2');
    void* saved_slot=input::mouse_slots[26];input::mouse_slots[26]=NULL;
    assert(input::control_at(env,"mouse_right_click 1 2",6700)[0]=='2');
    input::mouse_slots[26]=saved_slot;
    assert(events.empty() && attempts==saved_attempts && !input::mouse_held);

    // Wheel notches and camera-look deltas use the exact r208022 mouse symbol IDs and
    // the same CMessageInput::PostInputEvent slot as the native CMouse::Update.
    events.clear();input::physical=0;
    input::modifier_get_supported=input::modifier_set_supported=true;
    for(const char* bad: {"wheel","wheel 0","wheel 51","wheel -51","wheel 1 extra",
        "wheel +1","wheel 1.5","meta+wheel 1","ctrl+ctrl+wheel 1","mouse_look",
        "mouse_look 0 0","mouse_look 4096 0","mouse_look 0 -4096","mouse_look 1.5 0",
        "mouse_look 1","mouse_lookfoo 1 2"})
        assert(input::control_at(env,bad,7000)[0]=='2');
    assert(events.empty());
    assert(input::control_at(env,"wheel 3",7000)[0]=='0');
    assert(events.size()==1 && events[0].id==264 && events[0].state==1 && events[0].value==3);
    mouse_event(0,264,1,3,0);
    input::tick_at(env,7049);assert(events.size()==1);
    input::tick_at(env,7050);assert(events.size()==2);
    mouse_event(1,264,2,0,0);
    assert(input::control_at(env,"wheel -2",7060)[0]=='0');
    assert(events.size()==3 && events[2].id==265 && events[2].state==1 && events[2].value==-2);
    input::tick_at(env,7110);
    assert(events.size()==4 && events[3].id==265 && events[3].state==2 && events[3].value==0);
    mouse_event(3,265,2,0,0);
    // A chord applies its modifiers to the wheel event, then releases them immediately.
    events.clear();input::physical=0x200;
    assert(input::control_at(env,"ctrl+shift+wheel 1",7200)[0]=='0');
    assert(events.size()==5 && input::physical==0x200);
    event(0,28,true,0x211);event(1,41,true,0x233);
    mouse_event(2,264,1,1,0x233);
    event(3,41,false,0x211);event(4,28,false,0x200);
    input::tick_at(env,7249);assert(events.size()==5);
    input::tick_at(env,7250);
    assert(events.size()==6 && events[5].id==264 && events[5].state==2);
    mouse_event(5,264,2,0,0x200);

    // Mouse look presses Mouse2, emits bounded relative maxis_x/y deltas, then releases.
    events.clear();input::physical=0;
    assert(input::control_at(env,"mouse_look 120 -50",7300)[0]=='0');
    assert(events.size()==5 && input::mouse_held && !input::mouse_pending);
    assert(input::mouse_button==257);
    mouse_event(0,257,1,1,0);
    mouse_event(1,266,8,60,0);mouse_event(2,267,8,-25,0);
    mouse_event(3,266,8,60,0);mouse_event(4,267,8,-25,0);
    input::tick_at(env,7399);assert(events.size()==5);
    input::tick_at(env,7400);
    assert(events.size()==6 && !input::mouse_held);
    mouse_event(5,257,2,0,0);
    assert(input::control_at(env,"mouse_look 0 1",7401)[0]=='0');
    input::tick_at(env,7501);
    assert(!input::mouse_held);
    puts("input event bytes, synchronous modifier shadow/restoration, shared holds, bounds, rollback, wheel/look and deadline release passed");
}
