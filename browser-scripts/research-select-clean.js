(() => {
  const guard = window.__calvinoResearchSelections?.get(cfg.researchGuardId);
  if (!guard) return {cleaned:true,present:false};
  const receipt={cleaned:true,present:true,blocked:guard.blocked,sawPress:guard.sawPress,sawClick:guard.sawClick};
  guard.cleanup();
  return receipt;
})()
