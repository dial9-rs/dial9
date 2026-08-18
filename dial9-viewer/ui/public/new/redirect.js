(() => {
  const page = window.location.pathname.split("/").pop();
  window.location.replace(`/${page}${window.location.search}${window.location.hash}`);
})();
