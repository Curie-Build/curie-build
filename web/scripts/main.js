// Curie site — small interactions.
//
// 1. Mark the current nav link as active by matching pathname.
// 2. On the landing page, animate the Cm electron count and a Geiger-style
//    pulse on the element badge.

(function () {
  // ---- active nav link ----
  const here = location.pathname.replace(/\/index\.html$/, '/');
  document.querySelectorAll('nav.site a[href], .docs-nav a[href]').forEach((a) => {
    const href = a.getAttribute('href');
    if (!href) return;
    // normalise: treat "/" and "/index.html" as same
    const target = href.replace(/\/index\.html$/, '/');
    if (target === here || (target !== '/' && here.endsWith(target))) {
      a.classList.add('active');
    }
  });

  // ---- copy buttons inside <pre> code blocks ----
  document.querySelectorAll('.code pre, .term pre').forEach((pre) => {
    const wrap = pre.parentElement;
    if (!wrap) return;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'copy-btn';
    btn.setAttribute('aria-label', 'copy to clipboard');
    btn.textContent = 'copy';
    Object.assign(btn.style, {
      position: 'absolute',
      top: '0.45rem',
      right: '0.6rem',
      padding: '0.2rem 0.55rem',
      fontSize: '0.72rem',
      fontFamily: 'JetBrains Mono, monospace',
      color: 'var(--text-lo)',
      background: 'transparent',
      border: '1px solid var(--border)',
      borderRadius: '4px',
      cursor: 'pointer',
      transition: 'all 120ms',
    });
    btn.addEventListener('mouseenter', () => {
      btn.style.color = 'var(--accent)';
      btn.style.borderColor = 'var(--accent-dim)';
    });
    btn.addEventListener('mouseleave', () => {
      btn.style.color = 'var(--text-lo)';
      btn.style.borderColor = 'var(--border)';
    });
    btn.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(pre.innerText);
        btn.textContent = 'copied';
        setTimeout(() => (btn.textContent = 'copy'), 1200);
      } catch (_) {
        btn.textContent = 'err';
      }
    });
    wrap.style.position = 'relative';
    wrap.appendChild(btn);
  });
})();
