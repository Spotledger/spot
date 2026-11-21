export let Plan = /* @__PURE__ */ function(Plan) {
	Plan[Plan["Small"] = 1] = "Small";
	Plan[Plan["Large"] = 2] = "Large";
	return Plan;
}({});
export const chosen = (size) => size & Plan.Large;
