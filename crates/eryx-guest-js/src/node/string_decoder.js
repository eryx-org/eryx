/*! node:string_decoder, vendored by regen.sh. It contains:

string_decoder@1.3.0

Node.js is licensed for use as follows:

"""
Copyright Node.js contributors. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to
deal in the Software without restriction, including without limitation the
rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.
"""

This license applies to parts of Node.js originating from the
https://github.com/joyent/node repository:

"""
Copyright Joyent, Inc. and other Node contributors. All rights reserved.
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to
deal in the Software without restriction, including without limitation the
rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.
"""

---

safe-buffer@5.2.1

The MIT License (MIT)

Copyright (c) Feross Aboukhadijeh

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
*/
"use strict";var __m=(()=>{var x=(t=>typeof require<"u"?require:typeof Proxy<"u"?new Proxy(t,{get:(e,r)=>(typeof require<"u"?require:e)[r]}):t)(function(t){if(typeof require<"u")return require.apply(this,arguments);throw Error('Dynamic require of "'+t+'" is not supported')});var c=(t,e)=>()=>{try{return e||t((e={exports:{}}).exports,e),e.exports}catch(r){throw e=0,r}};var p=c((o,g)=>{var h=x("buffer"),i=h.Buffer;function d(t,e){for(var r in t)e[r]=t[r]}i.from&&i.alloc&&i.allocUnsafe&&i.allocUnsafeSlow?g.exports=h:(d(h,o),o.Buffer=a);function a(t,e,r){return i(t,e,r)}a.prototype=Object.create(i.prototype);d(i,a);a.from=function(t,e,r){if(typeof t=="number")throw new TypeError("Argument must not be a number");return i(t,e,r)};a.alloc=function(t,e,r){if(typeof t!="number")throw new TypeError("Argument must be a number");var s=i(t);return e!==void 0?typeof r=="string"?s.fill(e,r):s.fill(e):s.fill(0),s};a.allocUnsafe=function(t){if(typeof t!="number")throw new TypeError("Argument must be a number");return i(t)};a.allocUnsafeSlow=function(t){if(typeof t!="number")throw new TypeError("Argument must be a number");return h.SlowBuffer(t)}});var F=c(w=>{var f=p().Buffer,N=f.isEncoding||function(t){switch(t=""+t,t&&t.toLowerCase()){case"hex":case"utf8":case"utf-8":case"ascii":case"binary":case"base64":case"ucs2":case"ucs-2":case"utf16le":case"utf-16le":case"raw":return!0;default:return!1}};function C(t){if(!t)return"utf8";for(var e;;)switch(t){case"utf8":case"utf-8":return"utf8";case"ucs2":case"ucs-2":case"utf16le":case"utf-16le":return"utf16le";case"latin1":case"binary":return"latin1";case"base64":case"ascii":case"hex":return t;default:if(e)return;t=(""+t).toLowerCase(),e=!0}}function v(t){var e=C(t);if(typeof e!="string"&&(f.isEncoding===N||!N(t)))throw new Error("Unknown encoding: "+t);return e||t}w.StringDecoder=l;function l(t){this.encoding=v(t);var e;switch(this.encoding){case"utf16le":this.text=B,this.end=k,e=4;break;case"utf8":this.fillLast=T,e=4;break;case"base64":this.text=L,this.end=U,e=3;break;default:this.write=A,this.end=D;return}this.lastNeed=0,this.lastTotal=0,this.lastChar=f.allocUnsafe(e)}l.prototype.write=function(t){if(t.length===0)return"";var e,r;if(this.lastNeed){if(e=this.fillLast(t),e===void 0)return"";r=this.lastNeed,this.lastNeed=0}else r=0;return r<t.length?e?e+this.text(t,r):this.text(t,r):e||""};l.prototype.end=S;l.prototype.text=E;l.prototype.fillLast=function(t){if(this.lastNeed<=t.length)return t.copy(this.lastChar,this.lastTotal-this.lastNeed,0,this.lastNeed),this.lastChar.toString(this.encoding,0,this.lastTotal);t.copy(this.lastChar,this.lastTotal-this.lastNeed,0,t.length),this.lastNeed-=t.length};function u(t){return t<=127?0:t>>5===6?2:t>>4===14?3:t>>3===30?4:t>>6===2?-1:-2}function m(t,e,r){var s=e.length-1;if(s<r)return 0;var n=u(e[s]);return n>=0?(n>0&&(t.lastNeed=n-1),n):--s<r||n===-2?0:(n=u(e[s]),n>=0?(n>0&&(t.lastNeed=n-2),n):--s<r||n===-2?0:(n=u(e[s]),n>=0?(n>0&&(n===2?n=0:t.lastNeed=n-3),n):0))}function y(t,e,r){if((e[0]&192)!==128)return t.lastNeed=0,"\uFFFD";if(t.lastNeed>1&&e.length>1){if((e[1]&192)!==128)return t.lastNeed=1,"\uFFFD";if(t.lastNeed>2&&e.length>2&&(e[2]&192)!==128)return t.lastNeed=2,"\uFFFD"}}function T(t){var e=this.lastTotal-this.lastNeed,r=y(this,t,e);if(r!==void 0)return r;if(this.lastNeed<=t.length)return t.copy(this.lastChar,e,0,this.lastNeed),this.lastChar.toString(this.encoding,0,this.lastTotal);t.copy(this.lastChar,e,0,t.length),this.lastNeed-=t.length}function E(t,e){var r=m(this,t,e);if(!this.lastNeed)return t.toString("utf8",e);this.lastTotal=r;var s=t.length-(r-this.lastNeed);return t.copy(this.lastChar,0,s),t.toString("utf8",e,s)}function S(t){var e=t&&t.length?this.write(t):"";return this.lastNeed?e+"\uFFFD":e}function B(t,e){if((t.length-e)%2===0){var r=t.toString("utf16le",e);if(r){var s=r.charCodeAt(r.length-1);if(s>=55296&&s<=56319)return this.lastNeed=2,this.lastTotal=4,this.lastChar[0]=t[t.length-2],this.lastChar[1]=t[t.length-1],r.slice(0,-1)}return r}return this.lastNeed=1,this.lastTotal=2,this.lastChar[0]=t[t.length-1],t.toString("utf16le",e,t.length-1)}function k(t){var e=t&&t.length?this.write(t):"";if(this.lastNeed){var r=this.lastTotal-this.lastNeed;return e+this.lastChar.toString("utf16le",0,r)}return e}function L(t,e){var r=(t.length-e)%3;return r===0?t.toString("base64",e):(this.lastNeed=3-r,this.lastTotal=3,r===1?this.lastChar[0]=t[t.length-1]:(this.lastChar[0]=t[t.length-2],this.lastChar[1]=t[t.length-1]),t.toString("base64",e,t.length-r))}function U(t){var e=t&&t.length?this.write(t):"";return this.lastNeed?e+this.lastChar.toString("base64",0,3-this.lastNeed):e}function A(t){return t.toString(this.encoding)}function D(t){return t&&t.length?this.write(t):""}});return F();})();
export default __m;
export const { StringDecoder } = __m;
