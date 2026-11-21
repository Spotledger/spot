export let Direction = /* @__PURE__ */ function(Direction) {
	Direction[Direction["North"] = 1] = "North";
	Direction[Direction["South"] = 2] = "South";
	return Direction;
}({});
export const name = (code) => Direction[code] ?? "unknown";
