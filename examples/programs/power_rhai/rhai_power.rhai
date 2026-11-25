// A Bohío power source written in Rhai, served by `guatiao-remote-server` with guatiao's Rhai source and started by a node as a
// provider program (see docs/providers.md "A provider in another language"). It keeps one machine's power state in memory;
// a real one would call a BMC or a smart switch.

const VERSION = "1.0.0";

let power_switch = #{
    name: "Power switch, in Rhai",
    kinds: ["bohio_power"],
    now: "on",
    describe: || #{
        provider: "rhai-power",
        offers: ["on", "off", "cycle"],
        lacks: ["soft_off", "reset", "nmi"],
        available: true,
    },
    state: || #{ state: this.now, done: true },
    act: |action| {
        if action == "on" || action == "off" {
            this.now = action;
        } else if action != "cycle" {
            // A refusal in the words of the kind's contract: this source does not take that action.
            throw guatiao::refuse("unsupported", `the switch takes on, off and cycle, not ${action}`);
        }
        #{ state: this.now, done: true }
    },
};

#{ providers: #{ power_switch: power_switch } }
