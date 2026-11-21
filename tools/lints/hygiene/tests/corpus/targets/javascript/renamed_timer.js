// Invented fixture comment is intentionally ignored.
export function measure(begin, end, maximum) {
const difference=end-begin;
if(difference<0){return 0;}
if(difference>maximum){return maximum;}
return Math.round(difference);
}
export function nameMeasure(amount){
if(!Number.isFinite(amount)){return "missing";}
return `${Math.floor(amount)} ticks`;
}
