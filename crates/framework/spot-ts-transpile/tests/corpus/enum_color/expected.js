export let Color = /* @__PURE__ */ function(Color) {
	Color[Color["Red"] = 0] = "Red";
	Color[Color["Green"] = 1] = "Green";
	Color[Color["Blue"] = 2] = "Blue";
	return Color;
}({});
export const label = (c) => Color[c];
