// The sample app guest in Go (componentize-go + wit-bindgen-go), WP-STR-07.T1.
//
// The world is wit/world.wit (a copy of crates/framework/spot-wasm/wit/test-world.wit). Run
//
//	componentize-go --world test-world bindings --format
//	go mod tidy
//
// to generate the bindings; the generated package (imported below as `wit_component`) exposes
// the world's export `on-event`. The generated symbol names are produced by wit-bindgen-go from
// the world and may differ between toolchain versions: adjust the import and the method name to
// match `go tool componentize-go ... bindings` output.
//
// The behaviour is the sample app reduced to one DocType, one lifecycle step and one
// script-callable function. Hours are parsed to hundredths and formatted back in integer
// arithmetic (ADR-12: no floating point for quantities).
package main

import (
	"strconv"
	"strings"

	wit_component "wit_component"
)

// The bookable hours range, in hundredths (0.25 to 24.00).
const (
	hoursMin = 25
	hoursMax = 2400
)

// Component implements the world's exports.
type Component struct{}

// OnEvent is the world's export `on-event`.
func (Component) OnEvent(event string) (string, *wit_component.Error) {
	op := jsonField(event, "op")
	name := jsonField(event, "name")
	switch {
	case op == "step" && name == "sample:check_hours":
		return checkHours(jsonField(event, "hours"))
	case op == "call" && name == "sample:double_hours":
		return doubleHours(jsonField(event, "hours"))
	default:
		return "", &wit_component.Error{
			Code:    "sample.unknown_op",
			Message: "no operation " + op + ":" + name,
		}
	}
}

func checkHours(hours string) (string, *wit_component.Error) {
	value, ok := hundredths(hours)
	if !ok {
		return "", &wit_component.Error{Code: "sample.bad_hours", Message: "hours must be a decimal string"}
	}
	if value < hoursMin || value > hoursMax {
		return "", &wit_component.Error{
			Code:    "sample.hours_out_of_range",
			Message: "Hours must be between 0.25 and 24.",
		}
	}
	return `{"ok":true,"step":"sample:check_hours"}`, nil
}

func doubleHours(hours string) (string, *wit_component.Error) {
	value, ok := hundredths(hours)
	if !ok {
		return "", &wit_component.Error{Code: "sample.bad_hours", Message: "hours must be a decimal string"}
	}
	return `{"value":"` + formatHours(value*2) + `"}`, nil
}

// hundredths parses a non-negative decimal with at most two places into hundredths.
func hundredths(text string) (int, bool) {
	whole, fraction, _ := strings.Cut(text, ".")
	if whole == "" {
		return 0, false
	}
	value, err := strconv.Atoi(whole)
	if err != nil {
		return 0, false
	}
	if len(fraction) > 2 {
		return 0, false
	}
	if fraction != "" {
		scaled, err := strconv.Atoi((fraction + "00")[:2])
		if err != nil {
			return 0, false
		}
		return value*100 + scaled, true
	}
	return value * 100, true
}

// formatHours formats hundredths as a canonical two-place decimal.
func formatHours(hundredths int) string {
	return strconv.Itoa(hundredths/100) + "." + pad2(hundredths%100)
}

func pad2(value int) string {
	if value < 10 {
		return "0" + strconv.Itoa(value)
	}
	return strconv.Itoa(value)
}

// jsonField reads a JSON string field's value: "key":"value". The envelope carries only string
// values, so a small scanner keeps the guest free of a JSON dependency.
func jsonField(json, key string) string {
	needle := `"` + key + `":"`
	start := strings.Index(json, needle)
	if start < 0 {
		return ""
	}
	rest := json[start+len(needle):]
	end := strings.IndexByte(rest, '"')
	if end < 0 {
		return ""
	}
	return rest[:end]
}

func init() {
	wit_component.RegisterExports(Component{})
}

func main() {}
