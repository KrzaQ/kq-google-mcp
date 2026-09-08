import { createRouter, createWebHistory } from 'vue-router'
import { setUnauthorizedHandler } from '@/api/client'
import { useSession } from '@/stores/session'

const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', name: 'home', component: () => import('@/views/HomeView.vue') },
    {
      path: '/login',
      name: 'login',
      component: () => import('@/views/LoginView.vue'),
      meta: { public: true },
    },
    {
      path: '/connections',
      name: 'connections',
      component: () => import('@/views/ConnectionsView.vue'),
    },
    { path: '/tokens', name: 'tokens', component: () => import('@/views/TokensView.vue') },
    { path: '/activity', name: 'activity', component: () => import('@/views/ActivityView.vue') },
    {
      path: '/:pathMatch(.*)*',
      name: 'not-found',
      component: () => import('@/views/NotFoundView.vue'),
    },
  ],
})

router.beforeEach(async (to) => {
  if (to.meta.public) return true
  const session = useSession()
  if (!session.checked) await session.load()
  if (!session.me) return { name: 'login', query: { next: to.fullPath } }
  return true
})

// `/api/*` answers 401 rather than redirecting, so a session that expired
// while the tab sat open is noticed here: the page it happened on is what the
// login comes back to.
setUnauthorizedHandler(() => {
  const session = useSession()
  session.me = null
  session.checked = true
  const current = router.currentRoute.value
  if (current.name === 'login') return
  router.push({ name: 'login', query: { next: current.fullPath } })
})

export default router
