const buttons=document.querySelectorAll('.switch button');
const show=k=>{document.body.dataset.view=k;buttons.forEach(b=>b.setAttribute('aria-pressed',b.dataset.k===k))};
buttons.forEach(b=>b.onclick=()=>show(b.dataset.k));
show('rps');
