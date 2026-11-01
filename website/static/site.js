// The table of contents is a <details> element: open on wide screens, where
// it sits beside the text, and collapsed on narrow ones.
const toc = document.getElementById("toc");
if (toc) {
  const wide = matchMedia("(min-width: 901px)");
  const sync = () => {
    if (wide.matches) toc.open = true;
  };
  sync();
  wide.addEventListener("change", sync);
}
